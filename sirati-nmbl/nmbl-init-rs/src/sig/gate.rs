//! Boot-flow signature gate wrappers + the audit-vs-enforce policy (#19).
//!
//! Thin wrappers over the FROZEN [`crate::sig::verify`] pipeline that apply the
//! operator's `[signing]` posture to a verify result. This layer answers ONE
//! question — "given that verify said X, and the config posture is Y, does the
//! boot proceed?" — and returns a [`PolicyDecision`] the caller acts on. It
//! holds NO trust logic of its own: the cryptographic decision is entirely the
//! verify pipeline's; the gate only maps that decision through the
//! audit-vs-enforce knob.
//!
//! ## Audit vs enforce (FIX-04 / FIX-16 / FIX-31)
//!
//! Two postures, derived from `[signing]`:
//!
//! * **Enforce** (`signing.enforce`, the production default once
//!   `signing.enable`): a bad OR missing signature is fail-closed. The wrapper
//!   returns the verify `Err` unchanged; the Wave-2 caller (#20) routes that
//!   failure to `policy::refuse_unsigned` → `RebootIntoRescue` (R-1).
//! * **Audit** (`signing.enable && !signing.enforce`, itself gated by the
//!   separate `secureBoot.allowAuditModeInsecure` on the Nix side): the SAME
//!   verify runs, but a failure only WARNS and the boot proceeds. This is the
//!   ONLY relaxation of enforcement that exists.
//!
//! There is deliberately **NO allow-unsigned / skip-verification fork**
//! (FIX-04): [`apply_policy`] has no "don't verify" branch. The verify pipeline
//! always runs; audit mode only downgrades the *consequence* of a failure from
//! fail-closed to a warning. A future regression that tried to add a
//! skip-verify path would have to add a new variant here, which the tests pin
//! against.
//!
//! ## Seam for the Wave-2 pre-kexec guard (#20)
//!
//! [`apply_policy`] RETURNS the decision; it does NOT itself reboot, cap the
//! TPM, or construct a `TerminalAction`. The ENFORCE-fail ACTION
//! (`policy::refuse_unsigned` / `RebootIntoRescue`, which caps then closes
//! mappers then relocks before the refuse countdown) lives in the PARALLEL
//! `policy/relock` work (#26) and is invoked by #20. The contract at this seam
//! is:
//!
//! ```text
//! let decision = sig::apply_policy(config, verify_result);
//! match decision {
//!     PolicyDecision::Proceed => { /* boot continues */ }
//!     PolicyDecision::Refuse(cause) => return policy::refuse_unsigned(config, cause),
//! }
//! ```
//!
//! So the only routing #20 must do on a `Refuse` is hand `cause` to
//! `refuse_unsigned`; this module never does that itself.

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::sys::ops::FsOps;
use crate::{nmbl_info, nmbl_warn};

use super::verify::{self, VerifyPolicy};

// This module only ever compiles under `secure-boot` (it is `#[cfg]`-gated in
// the facade). Pin that here so a stub re-introduction can never coexist with
// the real gate (mirrors the verify.rs invariant — FIX-50).
const _: () = assert!(
    cfg!(feature = "secure-boot"),
    "sig::gate must only compile under the secure-boot feature (FIX-50)"
);

/// The boot-flow decision a signature gate produces.
///
/// Deliberately a two-variant outcome: a boot either proceeds or is refused.
/// There is NO third "skip / allow-unsigned" variant — that is the structural
/// guarantee behind FIX-04. Audit mode does not produce a distinct variant; it
/// collapses a verify failure into [`Proceed`](Self::Proceed) (after warning),
/// so the audit relaxation is visibly just "proceed anyway", never a bypass of
/// verification itself.
#[derive(Debug)]
pub enum PolicyDecision {
    /// The boot proceeds: either verification passed, or it failed under audit
    /// mode (a warning was logged) — never because verification was skipped.
    Proceed,
    /// Enforcement refused the boot. Carries the underlying verify error as the
    /// cause the Wave-2 caller (#20) hands to `policy::refuse_unsigned`. This
    /// module does NOT itself reboot or cap — see the module-level seam note.
    Refuse(NmblError),
}

impl PolicyDecision {
    /// `true` for [`Proceed`](Self::Proceed). Convenience for call sites and
    /// tests that only branch on proceed-vs-refuse.
    #[must_use]
    pub fn is_proceed(&self) -> bool {
        matches!(self, Self::Proceed)
    }
}

/// Apply the operator's `[signing]` posture to a verify result (FIX-04).
///
/// This is the ONE place audit-vs-enforce is resolved:
///
/// * `Ok(())` from verify ⇒ [`PolicyDecision::Proceed`] regardless of posture.
/// * `Err(e)` from verify ⇒
///   * **Enforce** ⇒ [`PolicyDecision::Refuse(e)`](PolicyDecision::Refuse) —
///     the caller routes `e` to `refuse_unsigned` (the seam for #20).
///   * **Audit** ⇒ WARN with `e` and return
///     [`PolicyDecision::Proceed`] — the boot continues, but the failure is on
///     the record. Reachable only behind the two-flag `enable && !enforce`
///     audit mode (FIX-16/FIX-31).
///
/// Crucially there is no branch that returns `Proceed` WITHOUT having a verify
/// result: every call site must have already run the verify pipeline. The gate
/// maps a *result*; it never stands in for one (no allow-unsigned — FIX-04).
#[must_use]
pub fn apply_policy(config: &Config, verify_result: Result<()>) -> PolicyDecision {
    match verify_result {
        Ok(()) => PolicyDecision::Proceed,
        Err(err) => match VerifyPolicy::from_config(config) {
            VerifyPolicy::Enforce => PolicyDecision::Refuse(err),
            // signing safety: the ONE audit-mode downgrade. Reachable only
            // behind the two-flag `enable && !enforce`, itself gated by the
            // Nix-side `allowAuditModeInsecure` (FIX-16/FIX-31) — never a
            // default. Warns loudly; a failed verify still proceeds because the
            // operator explicitly opted into insecure audit observation.
            VerifyPolicy::Audit => {
                nmbl_warn!(
                    "signature AUDIT mode: verification failed but boot proceeds (insecure): {err}"
                );
                PolicyDecision::Proceed
            }
        },
    }
}

/// Verify a generation's kernel+initrd signatures and apply the policy gate.
///
/// Thin composition: run the frozen [`verify::ensure_generation_signed`] over
/// the generation, then map its result through [`apply_policy`]. The Wave-2
/// pre-kexec guard (#20) calls this and routes a [`PolicyDecision::Refuse`] to
/// `policy::refuse_unsigned`; it is factored here so #20 holds no policy logic.
///
/// When signing is fully disabled (`!signing.enable`) the verify pipeline is
/// not meaningful, so this short-circuits to [`PolicyDecision::Proceed`]
/// WITHOUT pretending to have verified anything — the absence of any enabled
/// posture is the operator declining the feature, not an allow-unsigned bypass
/// of an enabled one (FIX-04).
#[must_use]
pub fn ensure_generation_signed_gated(
    fs: &dyn FsOps,
    config: &Config,
    generation: &Generation,
) -> PolicyDecision {
    // signing safety: signing-disabled is the operator declining the feature,
    // NOT an allow-unsigned bypass of an enabled one (FIX-04). No keys, no
    // posture, nothing to verify against; proceed without pretending to verify.
    if !config.signing.enable {
        nmbl_info!("signature verification disabled (signing.enable = false); skipping gate");
        return PolicyDecision::Proceed;
    }
    let result = verify::ensure_generation_signed(fs, config, generation);
    apply_policy(config, result)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on the gate-policy contract"
)]
mod tests {
    use super::*;

    /// Config with the given `enable`/`enforce` signing posture.
    fn config_with_posture(enable: bool, enforce: bool) -> Config {
        let text = format!(
            "[paths]\nshell = \"/bin/sh\"\n[signing]\nenable = {enable}\nenforce = {enforce}\n",
        );
        toml::from_str::<Config>(&text).expect("config parses")
    }

    fn bad_sig() -> NmblError {
        NmblError::Signature {
            stage: "test",
            detail: "crafted failure".to_string(),
        }
    }

    #[test]
    fn ok_result_always_proceeds() {
        // Pass verifies regardless of posture.
        for (en, enf) in [(false, false), (true, false), (true, true)] {
            let cfg = config_with_posture(en, enf);
            assert!(apply_policy(&cfg, Ok(())).is_proceed());
        }
    }

    #[test]
    fn enforce_mode_refuses_on_failure() {
        // enable=true, enforce=true ⇒ a verify failure is a hard Refuse that
        // carries the cause through to the caller (no allow-unsigned bypass).
        let cfg = config_with_posture(true, true);
        let decision = apply_policy(&cfg, Err(bad_sig()));
        match decision {
            PolicyDecision::Refuse(NmblError::Signature { stage: "test", .. }) => {}
            other => panic!("expected Refuse(Signature), got {other:?}"),
        }
    }

    #[test]
    fn enforce_default_when_signing_disabled_still_refuses_a_failure() {
        // With signing.enable=false the posture derives to Enforce (the
        // `enable && !enforce` audit gate is false), so a failure handed to
        // apply_policy is STILL a Refuse — there is no implicit allow-unsigned
        // for the disabled case (FIX-04). (The enable=false short-circuit lives
        // in `ensure_generation_signed_gated`, not in `apply_policy`.)
        let cfg = config_with_posture(false, false);
        assert!(matches!(
            apply_policy(&cfg, Err(bad_sig())),
            PolicyDecision::Refuse(_)
        ));
    }

    #[test]
    fn audit_mode_warns_but_proceeds_on_failure() {
        // enable=true, enforce=false ⇒ audit: a verify failure only warns and
        // the boot proceeds. This is the ONLY relaxation (FIX-16/FIX-31).
        let cfg = config_with_posture(true, false);
        assert!(
            apply_policy(&cfg, Err(bad_sig())).is_proceed(),
            "audit mode must proceed on a bad signature"
        );
    }

    #[test]
    fn no_skip_variant_exists() {
        // Structural guard against a re-introduced allow-unsigned bypass: the
        // decision is exhaustively proceed-or-refuse. A future skip variant
        // would force this match to stop compiling (FIX-04).
        let cfg = config_with_posture(true, true);
        match apply_policy(&cfg, Ok(())) {
            PolicyDecision::Proceed | PolicyDecision::Refuse(_) => {}
        }
    }

    #[test]
    fn gated_helper_short_circuits_when_disabled() {
        // signing.enable=false ⇒ the gated helper proceeds without invoking the
        // verify pipeline (which has no baked keys in this build). This is the
        // operator declining the feature, not a bypass.
        let cfg = config_with_posture(false, false);
        // A generation whose sidecars do not exist would FAIL verify if it ran;
        // proving we proceed shows the short-circuit fired before verify.
        let g = crate::generations::Generation {
            number: 1,
            profile_link: std::path::PathBuf::from("/nonexistent"),
            toplevel: std::path::PathBuf::from("/nonexistent"),
            kernel: std::path::PathBuf::from("/nonexistent/kernel"),
            initrd: std::path::PathBuf::from("/nonexistent/initrd"),
            init_path: std::path::PathBuf::from("/nonexistent/init"),
            kernel_params: Vec::new(),
            label: String::new(),
        };
        // The disabled short-circuit fires before any `fs` op, so a dry-run
        // ops over a non-existent closure root is never touched.
        let fs = dryrun_fs();
        assert!(ensure_generation_signed_gated(&fs, &cfg, &g).is_proceed());
    }

    /// A side-effect-free [`FsOps`] for the gate tests: a [`DryRunSys`] over a
    /// closure that is never read (the disabled short-circuit returns first).
    fn dryrun_fs() -> crate::sys::ops::dryrun::DryRunSys {
        use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};
        DryRunSys::new(
            ClosureView::new(std::path::PathBuf::from("/nonexistent")),
            DryRunScenario::NormalBoot,
        )
    }
}
