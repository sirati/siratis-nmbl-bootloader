//! The single decision whether a failed boot enters rescue automatically.
//!
//! Every "the boot failed and there is nothing left to fall back to" path
//! routes through [`on_boot_failure`]: a generation-image tested generation
//! that failed, stateful retry exhaustion, and every boot-phase error that
//! would otherwise open the emergency menu. Exactly one setting decides the
//! outcome, `[rescue].automatic` (Nix `boot.nmbl.rescue.automatic`):
//!
//! * `true`  -> [`FailureRoute::Rescue`]: enter the configured rescue
//!   (`[rescue].mode` selects WHICH rescue, never WHETHER).
//! * `false` -> [`FailureRoute::EmergencyMenu`]: the interactive menu.
//!
//! Two outcomes are deliberately NOT decided here:
//!
//! * The automatic rollback of a failed UNTESTED generation to its tested
//!   predecessor happens first, before any failure is declared, whenever a
//!   rollback target exists (generation-image state / stateful ring).
//! * Security refusals (a bad signature, the priority-file gate, a failed
//!   seal) take the refuse terminus: cap the TPM, relock, write the
//!   sentinel, reboot. That is a policy outcome, not availability recovery.
//!
//! Operator decisions (the operator chose reboot, aborted a wait, left a
//! wrong-password shell) are not boot failures and always return to the
//! menu. A failure that happens while rescue is already being entered is
//! never retried as rescue, so the decision cannot loop.

use crate::config::Config;
use crate::error::NmblError;

/// Where a failed boot goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureRoute {
    /// Enter the configured rescue without operator input.
    Rescue,
    /// Show the interactive emergency menu.
    EmergencyMenu,
}

/// Why the boot ended in failure. Recorded so logs name the path; the route
/// itself never depends on it except for the non-failure kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// A tested signed generation failed its previous boot
    /// (generation-image mode).
    TestedGenerationFailed,
    /// The stateful known-good ring and older generations are exhausted.
    StatefulExhausted,
    /// A boot phase failed (mount, activation, scan, kexec, config load,
    /// panic, ...).
    BootError,
}

/// Classify a boot-phase error. Operator decisions and failures of the
/// rescue path itself are not boot failures (see module docs).
#[must_use]
pub fn classify(err: &NmblError) -> Option<FailureKind> {
    match err {
        NmblError::OperatorAborted { .. }
        | NmblError::OperatorChoseReboot { .. }
        | NmblError::WrongPasswordShellExited { .. } => None,
        // Already on the rescue path: a second attempt would loop.
        NmblError::Rescue { stage, .. } if *stage != "stateful-exhausted" => None,
        NmblError::Rescue { .. } => Some(FailureKind::StatefulExhausted),
        _ => Some(FailureKind::BootError),
    }
}

/// The routing decision for `kind`, from `[rescue].automatic` alone.
#[must_use]
pub fn route(automatic: bool, kind: Option<FailureKind>) -> FailureRoute {
    match (automatic, kind) {
        (true, Some(_)) => FailureRoute::Rescue,
        _ => FailureRoute::EmergencyMenu,
    }
}

/// [`route`] for a failure of `kind` under `config`.
#[must_use]
pub fn on_boot_failure(config: &Config, kind: FailureKind) -> FailureRoute {
    route(config.rescue.automatic, Some(kind))
}

/// [`route`] for a boot-phase error under `config`.
#[must_use]
pub fn on_boot_error(config: &Config, err: &NmblError) -> FailureRoute {
    route(config.rescue.automatic, classify(err))
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert on contract failures")]
mod tests {
    use super::*;

    fn io_error() -> NmblError {
        NmblError::Io {
            source: std::io::Error::other("x"),
            context: "test".into(),
        }
    }

    fn every_failure() -> Vec<(&'static str, NmblError)> {
        vec![
            ("io", io_error()),
            (
                "device-timeout",
                NmblError::DeviceTimeout {
                    device: "/dev/vda".into(),
                    timeout_ms: 1,
                },
            ),
            ("no-generations", NmblError::NoGenerations { searched: "/p".into() }),
            (
                "bootstrap",
                NmblError::Bootstrap {
                    stage: "mount-boot",
                    source: Box::new(io_error()),
                },
            ),
            ("panicked", NmblError::Panicked { report_path: "/r".into() }),
            (
                "stateful-exhausted",
                NmblError::Rescue {
                    stage: "stateful-exhausted",
                    source: Box::new(io_error()),
                },
            ),
        ]
    }

    #[test]
    fn automatic_on_routes_every_failure_path_to_rescue() {
        for kind in [
            FailureKind::TestedGenerationFailed,
            FailureKind::StatefulExhausted,
            FailureKind::BootError,
        ] {
            assert_eq!(route(true, Some(kind)), FailureRoute::Rescue, "{kind:?}");
        }
        for (name, err) in every_failure() {
            assert_eq!(route(true, classify(&err)), FailureRoute::Rescue, "{name}");
        }
    }

    #[test]
    fn automatic_off_routes_every_failure_path_to_the_menu() {
        for kind in [
            FailureKind::TestedGenerationFailed,
            FailureKind::StatefulExhausted,
            FailureKind::BootError,
        ] {
            assert_eq!(route(false, Some(kind)), FailureRoute::EmergencyMenu, "{kind:?}");
        }
        for (name, err) in every_failure() {
            assert_eq!(route(false, classify(&err)), FailureRoute::EmergencyMenu, "{name}");
        }
    }

    #[test]
    fn operator_decisions_and_rescue_failures_never_enter_rescue() {
        let not_failures = [
            NmblError::OperatorAborted { context: "wait".into() },
            NmblError::OperatorChoseReboot { context: "modal".into() },
            NmblError::WrongPasswordShellExited { context: "shell".into() },
            NmblError::Rescue {
                stage: "disk-rescue-failed",
                source: Box::new(io_error()),
            },
        ];
        for err in &not_failures {
            assert_eq!(classify(err), None, "{err}");
            assert_eq!(route(true, classify(err)), FailureRoute::EmergencyMenu, "{err}");
        }
    }

    #[test]
    fn only_rescue_automatic_changes_the_route() {
        // The decision reads no setting besides `[rescue].automatic`: flipping
        // the rescue mode, force-on-boot, network rescue or generation policy
        // leaves the route unchanged.
        let mut config = Config::recovery_default();
        for automatic in [true, false] {
            config.rescue.automatic = automatic;
            let expected = route(automatic, Some(FailureKind::BootError));
            for mode in [
                crate::rescue::RescueMode::Embedded,
                crate::rescue::RescueMode::External,
                crate::rescue::RescueMode::None,
            ] {
                config.rescue.mode = mode;
                for force in [true, false] {
                    config.rescue.force_on_boot = force;
                    config.rescue.network = !force;
                    assert_eq!(on_boot_failure(&config, FailureKind::BootError), expected);
                    assert_eq!(on_boot_error(&config, &io_error()), expected);
                }
            }
        }
    }
}
