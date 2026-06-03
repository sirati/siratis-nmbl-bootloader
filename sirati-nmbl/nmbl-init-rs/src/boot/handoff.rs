//! Pre-kexec handoff staging: build the cmdline, stage the NMBL log
//! transcript into the next kernel's initramfs, assemble the cpio
//! fragment, and — the security keystone (#20) — VERIFY the generation's
//! signature, leave the PCR-11 MEASURE seam (#27, Wave-3), then LOAD the
//! image via `kexec_file_load(2)`. Split out of `boot/mod.rs` so the
//! verify/measure insertion points live next to the staging they gate.
//! Load MUST happen before any unmount —
//! [`crate::sys::kexec::load_with_extra_initrd_cpio`] reads kernel+initrd
//! from the still-mounted `/mnt/system`.
//!
//! ## verify → [measure seam] → load (FIX-02 / FIX-13 / FIX-14)
//!
//! [`verify_measure_then_load`] performs three steps in a FIXED order:
//!
//! 1. **VERIFY** the generation's kernel+initrd signatures BEFORE anything
//!    is loaded. The verify runs over PINNED file descriptors — each blob
//!    is opened ONCE inside [`crate::sig::verify::ensure_generation_signed`]
//!    and streamed through SHA-512 from that one fd, never re-opened by
//!    path for hashing (FIX-02). An enforce-mode failure is surfaced as
//!    [`NmblError::PolicyRefused`], which the `run_tui_session` Err arm maps
//!    to the [`RebootIntoRescue`] terminus — there is NO bypass and NO
//!    allow-unsigned path (R-1 / FIX-04). Audit mode (`signing.enable &&
//!    !enforce`) warns and proceeds, handled entirely inside the gate.
//! 2. **MEASURE seam** (`// #27 measure seam:`): the clearly-marked,
//!    well-structured insertion point for the Wave-3 PCR-11 extend
//!    (`tpm::measure::extend_handoff`). It sits AFTER verify and BEFORE load
//!    (FIX-14) and carries the byte-identical [`Handoff`] (the exact cmdline
//!    that will be loaded) so the value measured equals the value loaded.
//!    The extend itself is NOT implemented here (#27 owns it).
//! 3. **LOAD** the verified image with the SAME cmdline buffer that the
//!    measure seam saw (FIX-14): the `cmdline` passed to `kexec_file_load(2)`
//!    is byte-for-byte the `Handoff::cmdline` the seam carried.

use std::path::Path;

use crate::activation::KeyInjection;
use crate::config::Config;
use crate::error::Result;
use crate::generations::Generation;
use crate::imageload::DriverImagesHandle;
use crate::log;
use crate::sys::cpio::{InjectionEntry, build_fragment};
use crate::{nmbl_info, nmbl_warn};

#[cfg(feature = "secure-boot")]
pub(super) use crate::sig::VerifiedGeneration;

// Re-imported so the `#[path]`-included test module (a child of `handoff`) can
// reach the measure helpers that physically live in `handoff_load`.
#[cfg(test)]
use super::handoff_load::{measure_handoff, measure_required};

// When `secure-boot` is off there is no verify pipeline and no pinned fd; the
// witness type degrades to a unit so the single code path below still compiles.
#[cfg(not(feature = "secure-boot"))]
pub(super) type VerifiedGeneration = std::convert::Infallible;

// Tmpfs path the NMBL byte-ring is flushed to before kexec — recreated
// in the next kernel's initramfs by the cpio fragment we splice into
// `kexec_file_load(2)` below, so a stage-1 helper (e.g. `nmbl-log-import`)
// can pick the transcript up. Single source of truth in `log`.
use crate::log::NMBL_LOG_PATH;

/// Final cmdline.
///
/// * `cmdline_override` (TUI editor path) wins verbatim — an operator who has
///   hand-edited the line must not have their text silently mutated. No
///   `init=` injection happens in this branch.
/// * Otherwise the generation's own `kernel_params` are space-joined, and
///   `init=<stage2>` is appended unless the joined string already carries an
///   `init=` token (split on whitespace). The init value is the generation's
///   `init_path` stripped of `system_root`, with a leading `/` re-prepended so
///   the chained kernel — which mounts the store at `/`, not under our
///   `/mnt/system` prefix — sees a path that exists in its own namespace. If
///   `init_path` is somehow outside `system_root`, fall back to the raw path
///   with a warning rather than producing a broken cmdline.
fn build_cmdline(
    generation: &Generation,
    cmdline_override: Option<&str>,
    system_root: &Path,
) -> String {
    if let Some(s) = cmdline_override {
        return s.to_string();
    }

    let joined = generation.kernel_params.join(" ");
    if joined
        .split_ascii_whitespace()
        .any(|t| t.starts_with("init="))
    {
        return joined;
    }

    let init_arg = match generation.init_path.strip_prefix(system_root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => {
            nmbl_warn!(
                "init path {} is not under system_root {}; passing through unchanged",
                generation.init_path.display(),
                system_root.display(),
            );
            generation.init_path.display().to_string()
        }
    };

    if joined.is_empty() {
        format!("init={init_arg}")
    } else {
        format!("{joined} init={init_arg}")
    }
}

/// Persist the byte-ring transcript to NMBL_LOG_PATH and return the
/// resulting bytes for cpio injection. Failures degrade to an empty
/// transcript: we still want the kexec to fire, and the absence of an
/// `/nmbl-log/nmbl.log` entry in the next kernel's initramfs is a
/// recoverable diagnostic, not a boot-blocker. The `mkdir -p` of the
/// parent matches the same step in `execute_terminal_action`'s flush
/// so the file is reachable here even when the dispatcher flush in
/// `main` hasn't run yet (it runs after `kexec_into` returns).
fn stage_log_for_kexec() -> Vec<u8> {
    let log_path = Path::new(NMBL_LOG_PATH);
    if let Some(parent) = log_path.parent() {
        // EEXIST is benign; any harder failure surfaces through flush_to.
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(err) = log::flush_to(log_path) {
        nmbl_warn!(
            "kexec: failed to flush log to {} for staging: {err}",
            log_path.display()
        );
        return Vec::new();
    }
    match std::fs::read(log_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            nmbl_warn!(
                "kexec: failed to read flushed log at {} for staging: {err}",
                log_path.display()
            );
            Vec::new()
        }
    }
}

/// The byte-exact handoff inputs that bind verify, measure, and load.
///
/// Built AFTER the signature gate passes and carried THROUGH the `// #27
/// measure seam:` into the load. Holding the `cmdline` in one owned value
/// that both the (future) PCR-11 extend and the `kexec_file_load(2)` call
/// read from guarantees the cmdline measured is byte-identical to the
/// cmdline loaded (FIX-14): there is exactly one `String`, never two
/// independently-rebuilt copies that could drift.
struct Handoff {
    /// The final, NUL-free kernel cmdline — the SAME buffer the measure
    /// seam will hash and the load will pass to `kexec_file_load(2)`.
    cmdline: String,
}

/// VERIFY the generation's signature before anything is loaded (#20 step a),
/// returning the PINNED kernel fd + reused digests on the secure-boot path so
/// the measure and load steps consume the EXACT bytes that were verified
/// (FIX-02 / MED-1).
///
/// Runs the audit-vs-enforce gate over the generation. The verify streams each
/// blob through SHA-512 over a SINGLE pinned fd (FIX-02 — opened once inside the
/// verify pipeline, never re-opened by path for hashing) and applies the
/// operator's `[signing]` posture:
///
/// * **Proceed (verified)** — verification passed. Returns
///   `Ok(Some(VerifiedGeneration))`: the kernel fd is kept open for the load,
///   and the kernel+initrd digests are carried to the measure (no re-hash).
/// * **Proceed (no pin)** — signing is disabled, OR audit mode (`signing.enable
///   && !enforce`) downgraded a failure to a warning. Returns `Ok(None)`: there
///   is no verified fd to pin, so the load falls back to open-by-path and the
///   measure has no reused digest (an unverified/audit boot is not measured).
/// * **Refuse(cause)** — enforce mode rejected a bad/missing signature. Returns
///   `Err(PolicyRefused)` so the `run_tui_session` Err arm routes it to
///   `policy::run_refuse_screen` → [`RebootIntoRescue`] (R-1). There is NO
///   bypass and NO allow-unsigned branch here (FIX-04).
///
/// When the `secure-boot` feature is off there are no signatures to check, so
/// this is a no-op `Ok(None)` — the feature being absent is the operator
/// declining signature enforcement at build time, not a runtime bypass.
///
/// [`RebootIntoRescue`]: crate::terminal::TerminalAction::RebootIntoRescue
#[cfg_attr(
    not(feature = "secure-boot"),
    expect(
        clippy::unnecessary_wraps,
        unused_variables,
        reason = "no-op verify when secure-boot is disabled at build time"
    )
)]
fn verify_generation_signature(
    config: &Config,
    generation: &Generation,
) -> Result<Option<VerifiedGeneration>> {
    #[cfg(feature = "secure-boot")]
    {
        use crate::error::NmblError;
        use crate::sig::{VerifyPolicy, verify_generation_pinned};

        // signing safety: signing-disabled is the operator declining the
        // feature, NOT an allow-unsigned bypass of an enabled one (FIX-04).
        // Proceed with no pinned fd (the load opens by path, unmeasured).
        if !config.signing.enable {
            nmbl_info!("signature verification disabled (signing.enable = false); skipping gate");
            return Ok(None);
        }

        match verify_generation_pinned(config, generation) {
            // Verified: keep the pinned kernel fd + reused digests for
            // measure+load.
            Ok(verified) => Ok(Some(verified)),
            // A verify failure: map through the audit-vs-enforce posture.
            Err(err) => match VerifyPolicy::from_config(config) {
                // Enforce: hand the cause to the shared refuse terminus via
                // PolicyRefused. The `run_tui_session` Err arm (FIX-35) maps it
                // to the non-interactive refuse countdown + RebootIntoRescue —
                // NEVER the shell-offering emergency menu. Nothing is loaded.
                VerifyPolicy::Enforce => Err(NmblError::PolicyRefused {
                    cause: Box::new(err),
                }),
                // signing safety: the audit-mode downgrade (enable && !enforce,
                // gated by allowAuditModeInsecure — FIX-31) — the ONLY
                // relaxation. Warn and proceed unpinned + unmeasured; the
                // operator opted into insecure observation. Never a default.
                VerifyPolicy::Audit => {
                    nmbl_warn!(
                        "signature AUDIT mode: verification failed but boot proceeds (insecure, unmeasured): {err}"
                    );
                    Ok(None)
                }
            },
        }
    }
    #[cfg(not(feature = "secure-boot"))]
    {
        Ok(None)
    }
}

/// Build the cmdline, VERIFY the generation, leave the PCR-11 measure
/// seam (#27), then LOAD the image via `kexec_file_load(2)` — in that
/// FIXED order over the staged inputs. Returns the final cmdline so the
/// caller can log it before tearing the mounts down. The cutover syscall
/// stays in the dispatcher — this only fills the kexec image slot.
///
/// Verify happens FIRST, before the image is staged into the kexec slot:
/// an enforce-mode signature failure returns [`NmblError::PolicyRefused`]
/// and nothing is loaded (R-1 / FIX-04). The cmdline that the measure seam
/// sees and the cmdline handed to `kexec_file_load(2)` are the SAME owned
/// [`Handoff::cmdline`] buffer (FIX-14).
///
/// When `key_injections` is non-empty, an in-memory cpio fragment
/// containing those files is appended to the system initrd via
/// `memfd_create(2)` before `kexec_file_load(2)` — the typed
/// passphrases never touch disk.
///
/// [`NmblError::PolicyRefused`]: crate::error::NmblError::PolicyRefused
pub(crate) fn verify_measure_then_load(
    config: &Config,
    generation: &Generation,
    cmdline_override: Option<&str>,
    key_injections: &[KeyInjection],
    driver_images: &DriverImagesHandle,
) -> Result<String> {
    let handoff = Handoff {
        cmdline: build_cmdline(generation, cmdline_override, &config.paths.system_root),
    };
    nmbl_info!(
        "kexec: loading generation {} (kernel={}, initrd={})",
        generation.number,
        generation.kernel.display(),
        generation.initrd.display()
    );

    // (a) VERIFY the generation's kernel+initrd signatures over pinned
    // fds, BEFORE the image is staged into the kexec slot (FIX-02). An
    // enforce-mode failure short-circuits here with PolicyRefused — no
    // load, no bypass (R-1/FIX-04). Audit mode warns inside the gate and
    // returns Ok(None) so the boot proceeds unpinned + unmeasured. On the
    // secure-boot happy path we get back the PINNED kernel fd + reused
    // kernel/initrd digests, threaded into measure (no re-hash) and load
    // (no re-open) below (MED-1).
    let verified: Option<VerifiedGeneration> = verify_generation_signature(config, generation)?;

    // Stage the NMBL log transcript into the next kernel's initramfs.
    // The byte ring lives in RAM and the current tmpfs at NMBL_LOG_PATH
    // does not survive `reboot(LINUX_REBOOT_CMD_KEXEC)` — only what we
    // splice into the cpio fragment kexec_file_load(2) consumes reaches
    // the next kernel. We flush the ring to NMBL_LOG_PATH first (so the
    // helper that reads it back gets a header-aware snapshot identical
    // to the non-kexec terminal-action paths) and then read it back to
    // append as a cpio entry. Read failures degrade silently — the log
    // is best-effort and must never block the boot handoff.
    let log_bytes: Vec<u8> = stage_log_for_kexec();
    let log_path = Path::new(NMBL_LOG_PATH);

    let mut entries: Vec<InjectionEntry<'_>> = key_injections
        .iter()
        .map(|inj| InjectionEntry {
            path: inj.path.as_path(),
            content: inj.secret.as_slice(),
        })
        .collect();
    entries.push(InjectionEntry {
        path: log_path,
        content: log_bytes.as_slice(),
    });
    let fragment = build_fragment(&entries);
    if !key_injections.is_empty() {
        nmbl_info!(
            "kexec: injecting {} keyfile(s) + log into initrd via memfd ({} bytes)",
            key_injections.len(),
            fragment.len()
        );
    } else {
        nmbl_info!(
            "kexec: injecting log into initrd via memfd ({} bytes)",
            fragment.len()
        );
    }

    // (b) #27/#28 MEASURE: extend PCR-11 with the verified handoff HERE — AFTER
    // verify, BEFORE load (FIX-14). The extend reuses the kernel+initrd
    // digests the verify already streamed over the pinned fds (FIX-02 — no
    // re-hash), binds `handoff.cmdline` (the byte-exact buffer the load below
    // consumes), and folds the ORDERED driver-image refs the loader verified
    // (#28 — name‖digest per loaded image, no re-hash). Gated on the measure
    // posture: a NO-OP when measuring is off; a measure failure on a
    // measure-required build fails CLOSED (routes to refuse, never an unmeasured
    // boot — FIX-27).
    super::handoff_load::measure_handoff(
        config,
        generation,
        verified.as_ref(),
        &handoff.cmdline,
        driver_images,
    )?;

    let Handoff { cmdline } = handoff;

    // (c) LOAD the verified image with the SAME cmdline the seam carried
    // (FIX-14) and — on the secure-boot path — the SAME pinned kernel fd
    // that was verified and measured (FIX-02 / MED-1), so the loaded kernel
    // is byte-identical to the verified+measured one. When there is no
    // verified fd (non-secure-boot / audit), the loader opens by path.
    super::handoff_load::load_handoff(generation, verified, fragment.as_slice(), &cmdline)?;
    Ok(cmdline)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
#[path = "handoff_tests.rs"]
mod tests;
