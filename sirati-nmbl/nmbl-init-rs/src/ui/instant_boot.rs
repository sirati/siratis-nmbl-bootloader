//! Instant-boot decision (`boot.nmbl.instantBoot`).
//!
//! When every health condition holds AND no key was pressed during early
//! boot, NMBL skips the selector countdown entirely and boots the default
//! generation immediately. In every other case it falls back to the normal
//! selector timer.
//!
//! This module is the PURE decision core: [`InstantBootInputs`] carries the
//! facts NMBL gathers during early boot, and [`decide_instant_boot`] turns
//! them into a [`InstantBootDecision`]. Keeping the policy free of I/O makes
//! the health-conditions × key-pressed matrix fully unit-testable without a
//! console, a runtime, or a VM (Feature 2's decision-function test).
//!
//! ## Conditions (all must hold to instant-boot)
//!
//! 1. `enabled` — the operator set `boot.nmbl.instantBoot.enable`.
//! 2. `last_boot_succeeded` — the previous boot reached its success mark.
//! 3. Stateful safety: if stateful tracking is active, we must be booting
//!    the operator's default (not a `ForcePick` rollback to an older
//!    generation) — represented by `stateful_rollback_active = false`.
//! 4. `generation_rollback_active == false` — the signed-EROFS state machine
//!    did not roll an untested image back this boot.
//! 5. `rescue_sentinel_present == false` — no forced-rescue marker.
//! 6. `pending_untested_generation == false` — no untested generation the
//!    policy would otherwise want to show the operator.
//! 7. `key_pressed_during_early_boot == false` — the operator did NOT touch
//!    the keyboard/serial line while NMBL was reaching the selector.
//!
//! Condition 7 is the escape hatch: a single keypress anywhere between
//! stage 0 and the selector cancels the instant boot and drops the operator
//! into the normal menu, exactly as pressing a key during the countdown
//! would.

/// The facts that drive the instant-boot decision, gathered during early
/// boot. Every field is a plain bool so the decision is a pure predicate
/// over a small, exhaustively-testable input space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstantBootInputs {
    /// `boot.nmbl.instantBoot.enable` — the master opt-in.
    pub enabled: bool,
    /// The previous boot set its success mark (stateful `last_boot_succeeded`
    /// or the signed-generation `tested` selector). `false` when we cannot
    /// prove the last boot was healthy, which conservatively disables the
    /// instant boot.
    pub last_boot_succeeded: bool,
    /// Stateful tracking is active AND this boot is a rollback/fallback to a
    /// previous generation (a `ForcePick`), rather than the operator's
    /// default. `false` when stateful is off or we are booting the default.
    pub stateful_rollback_active: bool,
    /// The signed-EROFS generation state machine rolled an untested image
    /// back this boot (`BootStateOutcome::RolledBack`).
    pub generation_rollback_active: bool,
    /// The rescue sentinel file is present (an explicit force-rescue request).
    pub rescue_sentinel_present: bool,
    /// A pending, untested generation exists that the boot policy would
    /// otherwise want to surface to the operator.
    pub pending_untested_generation: bool,
    /// Any key was pressed on the console (or serial line) between early
    /// stage 0 and the selector.
    pub key_pressed_during_early_boot: bool,
}

impl InstantBootInputs {
    /// A conservative baseline: disabled, healthy, nothing pending, no key.
    /// Tests and callers build on this with struct-update syntax so a new
    /// field never silently defaults to the permissive value.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            last_boot_succeeded: true,
            stateful_rollback_active: false,
            generation_rollback_active: false,
            rescue_sentinel_present: false,
            pending_untested_generation: false,
            key_pressed_during_early_boot: false,
        }
    }
}

/// The outcome of the instant-boot policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstantBootDecision {
    /// Skip the selector countdown and boot the default generation now.
    BootImmediately,
    /// Run the normal selector timer / menu. Carries the reason so the log
    /// line and the tests can explain WHY the instant boot was declined.
    UseNormalTimer(InstantBootDeclineReason),
}

impl InstantBootDecision {
    /// Whether this decision is [`InstantBootDecision::BootImmediately`].
    #[must_use]
    pub fn is_immediate(self) -> bool {
        matches!(self, InstantBootDecision::BootImmediately)
    }
}

/// Why the instant boot was declined (i.e. why the normal timer applies).
/// Ordered by the checks in [`decide_instant_boot`]; the first failing
/// condition wins so the reason is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstantBootDeclineReason {
    /// `boot.nmbl.instantBoot.enable` is off.
    Disabled,
    /// The previous boot did not record a success mark.
    LastBootNotHealthy,
    /// Stateful rollback/fallback to a previous generation is in effect.
    StatefulRollbackActive,
    /// The signed-generation state machine rolled an untested image back.
    GenerationRollbackActive,
    /// The rescue sentinel forced a rescue boot.
    RescueSentinelPresent,
    /// A pending untested generation must be shown.
    PendingUntestedGeneration,
    /// The operator pressed a key during early boot.
    KeyPressedDuringEarlyBoot,
}

impl InstantBootDeclineReason {
    /// A short, log-friendly explanation of the decline reason.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InstantBootDeclineReason::Disabled => "instant boot is disabled",
            InstantBootDeclineReason::LastBootNotHealthy => {
                "the previous boot did not record a success mark"
            }
            InstantBootDeclineReason::StatefulRollbackActive => {
                "stateful rollback to a previous generation is active"
            }
            InstantBootDeclineReason::GenerationRollbackActive => {
                "an untested generation was rolled back this boot"
            }
            InstantBootDeclineReason::RescueSentinelPresent => "the rescue sentinel is present",
            InstantBootDeclineReason::PendingUntestedGeneration => {
                "a pending untested generation must be shown"
            }
            InstantBootDeclineReason::KeyPressedDuringEarlyBoot => {
                "a key was pressed during early boot"
            }
        }
    }
}

/// Decide whether to boot immediately or fall back to the normal timer.
///
/// Pure function over [`InstantBootInputs`]. The checks run in the fixed
/// order of [`InstantBootDeclineReason`]; the FIRST failing condition
/// determines the decline reason, so the outcome is deterministic and the
/// log line names the single most fundamental cause. `BootImmediately` is
/// returned ONLY when every condition holds.
#[must_use]
pub fn decide_instant_boot(inputs: InstantBootInputs) -> InstantBootDecision {
    use InstantBootDecision::UseNormalTimer as Decline;
    use InstantBootDeclineReason as R;

    if !inputs.enabled {
        return Decline(R::Disabled);
    }
    if !inputs.last_boot_succeeded {
        return Decline(R::LastBootNotHealthy);
    }
    if inputs.stateful_rollback_active {
        return Decline(R::StatefulRollbackActive);
    }
    if inputs.generation_rollback_active {
        return Decline(R::GenerationRollbackActive);
    }
    if inputs.rescue_sentinel_present {
        return Decline(R::RescueSentinelPresent);
    }
    if inputs.pending_untested_generation {
        return Decline(R::PendingUntestedGeneration);
    }
    if inputs.key_pressed_during_early_boot {
        return Decline(R::KeyPressedDuringEarlyBoot);
    }
    InstantBootDecision::BootImmediately
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert on contract failures")]
mod tests {
    use super::*;

    /// The all-conditions-hold baseline: every health condition satisfied,
    /// enabled, no key. This is the ONLY input that must boot immediately.
    fn all_healthy() -> InstantBootInputs {
        InstantBootInputs {
            enabled: true,
            ..InstantBootInputs::disabled()
        }
    }

    #[test]
    fn boots_immediately_when_everything_is_healthy_and_no_key() {
        assert_eq!(
            decide_instant_boot(all_healthy()),
            InstantBootDecision::BootImmediately,
        );
    }

    #[test]
    fn declines_when_disabled_even_if_healthy() {
        let inputs = InstantBootInputs {
            enabled: false,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::Disabled),
        );
    }

    #[test]
    fn declines_when_last_boot_not_healthy() {
        let inputs = InstantBootInputs {
            last_boot_succeeded: false,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::LastBootNotHealthy),
        );
    }

    #[test]
    fn declines_on_stateful_rollback() {
        let inputs = InstantBootInputs {
            stateful_rollback_active: true,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::StatefulRollbackActive),
        );
    }

    #[test]
    fn declines_on_generation_rollback() {
        let inputs = InstantBootInputs {
            generation_rollback_active: true,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::GenerationRollbackActive),
        );
    }

    #[test]
    fn declines_when_rescue_sentinel_present() {
        let inputs = InstantBootInputs {
            rescue_sentinel_present: true,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::RescueSentinelPresent),
        );
    }

    #[test]
    fn declines_on_pending_untested_generation() {
        let inputs = InstantBootInputs {
            pending_untested_generation: true,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(
                InstantBootDeclineReason::PendingUntestedGeneration
            ),
        );
    }

    #[test]
    fn declines_when_a_key_was_pressed_during_early_boot() {
        // The crucial escape hatch: even a fully healthy system must fall
        // back to the menu when the operator pressed any key early.
        let inputs = InstantBootInputs {
            key_pressed_during_early_boot: true,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(
                InstantBootDeclineReason::KeyPressedDuringEarlyBoot
            ),
        );
    }

    #[test]
    fn a_keypress_overrides_every_healthy_condition() {
        // Exhaustive belt-and-braces: for the healthy baseline, flipping the
        // key bit alone must always flip the decision away from immediate.
        let healthy = all_healthy();
        assert!(decide_instant_boot(healthy).is_immediate());
        let pressed = InstantBootInputs {
            key_pressed_during_early_boot: true,
            ..healthy
        };
        assert!(!decide_instant_boot(pressed).is_immediate());
    }

    #[test]
    fn decline_reason_is_the_first_failing_condition() {
        // When several conditions fail at once, the reason is the earliest
        // in the fixed check order (here: disabled beats the key press).
        let inputs = InstantBootInputs {
            enabled: false,
            key_pressed_during_early_boot: true,
            last_boot_succeeded: false,
            ..all_healthy()
        };
        assert_eq!(
            decide_instant_boot(inputs),
            InstantBootDecision::UseNormalTimer(InstantBootDeclineReason::Disabled),
        );
    }

    #[test]
    fn full_health_matrix_only_all_true_boots() {
        // Enumerate the 2^5 health-condition combinations (enabled fixed on,
        // key fixed off) and assert BootImmediately happens iff every health
        // condition is in its permissive state.
        for bits in 0u8..32 {
            let last_ok = bits & 1 != 0;
            let stateful_rb = bits & 2 != 0;
            let gen_rb = bits & 4 != 0;
            let sentinel = bits & 8 != 0;
            let pending = bits & 16 != 0;
            let inputs = InstantBootInputs {
                enabled: true,
                last_boot_succeeded: last_ok,
                stateful_rollback_active: stateful_rb,
                generation_rollback_active: gen_rb,
                rescue_sentinel_present: sentinel,
                pending_untested_generation: pending,
                key_pressed_during_early_boot: false,
            };
            let healthy = last_ok && !stateful_rb && !gen_rb && !sentinel && !pending;
            assert_eq!(
                decide_instant_boot(inputs).is_immediate(),
                healthy,
                "bits={bits:05b} expected immediate={healthy}",
            );
        }
    }
}
