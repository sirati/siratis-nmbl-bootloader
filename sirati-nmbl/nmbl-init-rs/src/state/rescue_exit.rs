//! Rescue intent never authorizes a retry; only a confirmed live console does.
use super::{read, write_padded};
use crate::error::Result;
use std::path::Path;

/// PID 1 calls this only after the trusted console launcher signals readiness.
/// False means supported state was unavailable; preserve the rescue sentinel.
pub fn record_rescue_booted(path: &Path) -> Result<bool> {
    let Some(mut state) = read(path)? else {
        return Ok(false);
    };
    state.rescue_booted_generation = if state.last_boot_succeeded {
        None
    } else {
        state.last_attempted_generation
    };
    state.rescue_exit_retry_in_progress = false;
    write_padded(path, &state)?;
    Ok(true)
}

/// Consume durably BEFORE dispatch. Stale targets cannot inherit authorization.
/// Ordinary dispatch still performs all configured signature checks.
pub fn take_rescue_exit_retry(path: &Path, available: &[u32]) -> Result<Option<u32>> {
    let Some(mut state) = read(path)? else {
        return Ok(None);
    };
    let Some(generation) = state.rescue_booted_generation.take() else {
        return Ok(None);
    };
    let number = generation.get();
    let retry = !state.last_boot_succeeded
        && state.last_attempted_generation == Some(generation)
        && available.contains(&number);
    if retry {
        state.rescue_exit_retry_in_progress = true;
    }
    write_padded(path, &state)?;
    Ok(retry.then_some(number))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert persistent cycle contracts"
)]
mod tests {
    use super::*;
    use crate::state::{State, StatefulDecision, decide, mark_boot_succeeded};
    use nonmax::NonMaxU32;

    #[test]
    fn rescue_ready_power_cycle_retries_once_then_failure_returns_to_rescue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.bin");
        let mut state = State {
            last_boot_succeeded: false,
            last_attempted_generation: NonMaxU32::new(42),
            recovery_attempt: 2,
            ..State::default()
        };
        state.known_good_generations[0] = NonMaxU32::new(7);
        write_padded(&path, &state).unwrap();
        // A rescue launch failure has no readiness event and cannot arm retry.
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), None);
        assert!(record_rescue_booted(&path).unwrap());
        let armed = read(&path).unwrap().unwrap();
        assert_eq!(armed.rescue_booted_generation, NonMaxU32::new(42));
        assert_eq!(armed.recovery_attempt, 2);
        assert_eq!(armed.known_good_generations, state.known_good_generations);
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), Some(42));
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), None);
        let mut failed_retry = read(&path).unwrap().unwrap();
        assert!(!failed_retry.last_boot_succeeded);
        // It must return to rescue even below the configured rollback limit.
        assert_eq!(
            decide(&mut failed_retry, &[], 0, 20),
            StatefulDecision::Exhausted
        );
        assert_eq!(failed_retry.recovery_attempt, 2);
        // A second actually ready rescue permits one new power-cycle attempt.
        assert!(record_rescue_booted(&path).unwrap());
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), Some(42));
        mark_boot_succeeded(dir.path()).unwrap();
        let healthy = read(&path).unwrap().unwrap();
        assert!(healthy.last_boot_succeeded);
        assert!(!healthy.rescue_exit_retry_in_progress);
        assert_eq!(healthy.rescue_booted_generation, None);
    }

    #[test]
    fn healthy_manual_rescue_and_removed_generation_never_authorize_failed_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.bin");
        let state = State {
            last_attempted_generation: NonMaxU32::new(42),
            ..State::default()
        };
        write_padded(&path, &state).unwrap();
        assert!(record_rescue_booted(&path).unwrap());
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), None);
        let mut failed = state;
        failed.last_boot_succeeded = false;
        write_padded(&path, &failed).unwrap();
        record_rescue_booted(&path).unwrap();
        assert_eq!(take_rescue_exit_retry(&path, &[]).unwrap(), None);
        assert_eq!(take_rescue_exit_retry(&path, &[42]).unwrap(), None);
    }
}
