//! [`DryRunScenario`]: which boot path `--validate-initrm` exercises, and
//! the scripted [`ProcessOutcome`] each scenario fabricates for an exec.
//!
//! The actual scenario DRIVERS (running the boot core once per variant)
//! land in the next phase. This module only supplies the enum and the
//! data-driven scripted-outcome logic [`super::DryRunSys`] needs so that,
//! when it dry-runs an activation or capture, it returns the outcome the
//! chosen scenario implies — e.g. `NormalBoot` always succeeds so the
//! boot walks straight to kexec, while `ErrorToErrorScreen` fails an
//! activation so the boot routes to the error screen.

use crate::sys::activation::ProcessOutcome;

/// Which boot path the dry-run is validating.
///
/// Each variant scripts a different exec-outcome policy so the next
/// phase can drive the genuine boot core four times over the SAME
/// closure and confirm every reachable path's file deps are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DryRunScenario {
    /// Happy path: every activation/exec "succeeds" (exit 0). The boot
    /// walks mount → activate → kexec.
    NormalBoot,
    /// An activation "fails" (non-zero exit) so the boot routes to the
    /// error screen. Validates the error-screen + recovery file deps.
    ErrorToErrorScreen,
    /// Operator drops to the splash/pretty emergency shell. Validates the
    /// pretty-shell preflight file deps (shell binary, devpts, ptmx).
    PrettyShell,
    /// Operator drops to the raw tty emergency shell. Same shell-spawn
    /// preflight deps, raw-tty variant.
    RawShell,
}

/// Whether an exec the dry-run is fabricating is an ACTIVATION (whose
/// failure routes the boot) or a neutral capture/probe (whose outcome
/// the scenario does not script to fail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRole {
    /// A boot-critical activation (luks/lvm/mdraid/zpool). Its non-zero
    /// exit under [`DryRunScenario::ErrorToErrorScreen`] is what routes
    /// the boot to the error screen.
    Activation,
    /// A neutral probe/capture (e.g. `blkid`). Always "succeeds" so a
    /// scenario never spuriously fails on a read-only probe.
    Probe,
}

impl DryRunScenario {
    /// The [`ProcessOutcome`] this scenario fabricates for an exec of the
    /// given [`ExecRole`]. `NormalBoot`/`PrettyShell`/`RawShell` always
    /// succeed (the shell scenarios diverge at `spawn_shell`, not at the
    /// activation execs); `ErrorToErrorScreen` fails an `Activation` so
    /// the boot routes to the error screen, but lets `Probe`s pass so a
    /// `blkid` capture still returns usable output.
    #[must_use]
    pub fn scripted_outcome(self, role: ExecRole) -> ProcessOutcome {
        match (self, role) {
            (DryRunScenario::ErrorToErrorScreen, ExecRole::Activation) => Self::failure(),
            _ => Self::success(),
        }
    }

    /// A clean exit-0 outcome.
    #[must_use]
    pub fn success() -> ProcessOutcome {
        ProcessOutcome {
            exit_code: 0,
            normal_exit: true,
        }
    }

    /// A fatal non-zero exit (mirrors a tool that failed but exited
    /// normally — the common "wrong passphrase / no such VG" shape).
    #[must_use]
    pub fn failure() -> ProcessOutcome {
        ProcessOutcome {
            exit_code: 1,
            normal_exit: true,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn normal_boot_activation_succeeds() {
        let o = DryRunScenario::NormalBoot.scripted_outcome(ExecRole::Activation);
        assert_eq!(o.exit_code, 0);
        assert!(o.normal_exit);
    }

    #[test]
    fn error_scenario_fails_activation_but_not_probe() {
        let act = DryRunScenario::ErrorToErrorScreen.scripted_outcome(ExecRole::Activation);
        assert_ne!(
            act.exit_code, 0,
            "activation must fail to route to error screen"
        );
        let probe = DryRunScenario::ErrorToErrorScreen.scripted_outcome(ExecRole::Probe);
        assert_eq!(probe.exit_code, 0, "a read-only probe must not be failed");
    }

    #[test]
    fn outcomes_differ_between_normal_and_error() {
        let normal = DryRunScenario::NormalBoot.scripted_outcome(ExecRole::Activation);
        let error = DryRunScenario::ErrorToErrorScreen.scripted_outcome(ExecRole::Activation);
        assert_ne!(normal, error);
    }

    #[test]
    fn shell_scenarios_pass_activations() {
        // The shell scenarios diverge at spawn_shell, not at activation
        // execs, so an activation in either still "succeeds".
        for s in [DryRunScenario::PrettyShell, DryRunScenario::RawShell] {
            assert_eq!(s.scripted_outcome(ExecRole::Activation).exit_code, 0);
        }
    }
}
