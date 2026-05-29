use std::path::Path;

use nmbl_init::config::Config;
use nmbl_init::error::{NmblError, Result};
use nmbl_init::generations::{Generation, active_generation_index};
use nmbl_init::ui::console::Console;
use nmbl_init::ui::{Decision, SessionInteraction, run_selector};
use nmbl_init::{nmbl_info, nmbl_warn};

/// Stateful entry point for the boot selector. Returns the same
/// [`Decision`] shape `run_selector` would have returned; the caller's
/// match on `Decision::Boot` / `Shell` / `Reboot` does not change.
///
/// Decision tree:
///   - No `[stateful]` table, or no `[bootstrap.state]` mount, or
///     no readable `state.bin`: fall back to `run_selector` unchanged.
///   - `state::decide` → `HonourTui`: call `run_selector`, then record
///     the operator's pick in `state.bin` before returning.
///   - `state::decide` → `ForcePick(idx)`: skip the TUI, synthesize a
///     `Decision::Boot` for `generations[idx]`, record the pick in
///     `state.bin`.
///   - `state::decide` → `Exhausted`: surface as
///     `NmblError::Rescue { stage: "stateful-exhausted", ... }` so
///     `run_inner`'s existing error arm routes through the emergency
///     screen.
pub(super) async fn select_with_stateful(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
    session: &SessionInteraction,
) -> Result<Decision> {
    // No opt-in: legacy path verbatim.
    let (Some(_stateful), Some(state_mp)) = (
        config.stateful.as_ref(),
        config.runtime_state_mountpoint.as_deref(),
    ) else {
        nmbl_info!("phase 5: TUI generation selector");
        return run_selector(config, generations, console, session).await;
    };

    let state_path = state_mp.join("nmbl").join("state.bin");
    let mut state = match nmbl_init::state::read(&state_path) {
        Ok(Some(s)) => s,
        Ok(None) => {
            // File missing or wire-format version newer than us. Either
            // is an explicit "fall back to non-stateful" signal per the
            // forward-compat contract on `State`; do not surface as
            // failure, just skip the rollback flow this boot.
            nmbl_warn!(
                "state.bin at {} absent or unsupported; skipping stateful boot this cycle",
                state_path.display(),
            );
            nmbl_info!("phase 5: TUI generation selector");
            return run_selector(config, generations, console, session).await;
        }
        Err(err) => {
            // IO error other than NotFound (which `read` already maps to
            // Ok(None)). The operator's choice was to enable stateful;
            // surfacing this as a hard rescue would be heavy-handed for
            // what may be a transient FS hiccup, so the contract is to
            // warn and skip — same fall-back as a missing file.
            nmbl_warn!(
                "state.bin at {} could not be read ({err}); skipping stateful boot this cycle",
                state_path.display(),
            );
            nmbl_info!("phase 5: TUI generation selector");
            return run_selector(config, generations, console, session).await;
        }
    };

    // Already validated at TOML parse time that `stateful = Some(...)`
    // means max_recovery_attempts is present.
    let max_attempts = _stateful.max_recovery_attempts;
    let active_index = active_generation_index(generations, &config.paths.nix_profiles_dir);

    match nmbl_init::state::decide(&mut state, generations, active_index, max_attempts) {
        nmbl_init::state::StatefulDecision::HonourTui => {
            nmbl_info!(
                "phase 5: TUI generation selector (stateful: honour operator choice, recovery_attempt={})",
                state.recovery_attempt,
            );
            let decision = run_selector(config, generations, console, session).await?;
            if let Decision::Boot {
                generation_index,
                cmdline_override: _,
            } = &decision
            {
                record_attempt(&mut state, generations, *generation_index, &state_path);
            }
            Ok(decision)
        }
        nmbl_init::state::StatefulDecision::ForcePick(idx) => {
            let Some(target) = generations.get(idx) else {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "state::decide returned ForcePick({idx}) but only {} generations",
                        generations.len()
                    ),
                    context: "stateful dispatch".to_string(),
                });
            };
            nmbl_info!(
                "phase 5: stateful rollback forced generation {} (recovery_attempt={})",
                target.number,
                state.recovery_attempt,
            );
            record_attempt(&mut state, generations, idx, &state_path);
            Ok(Decision::Boot {
                generation_index: idx,
                cmdline_override: None,
            })
        }
        nmbl_init::state::StatefulDecision::Exhausted => {
            // The emergency menu reads the source chain via
            // `format_chain`, so wrap a leaf error that explains *why*
            // the rescue arm fired. There's no `NmblError::Other`
            // variant; the existing pattern (e.g. `select_and_act`'s
            // `Decision::Shell` arm) wraps a free-form message in
            // `NmblError::Io` via `io::Error::other`. Reusing that here
            // keeps the chain walker happy and the operator-facing
            // string clear.
            Err(NmblError::Rescue {
                stage: "stateful-exhausted",
                source: Box::new(NmblError::Io {
                    source: std::io::Error::other(
                        "max recovery attempts exceeded; no known-good generation left to try",
                    ),
                    context: "stateful dispatch".to_string(),
                }),
            })
        }
    }
}

/// Persist the operator's (or stateful dispatcher's) generation pick to
/// `state.bin` before kexec. Write failures degrade to a warning — the
/// next boot will retry the decision against a stale state.bin, which
/// is strictly less bad than blocking the boot handoff. The `u32::MAX`
/// edge case on `NonMaxU32::new` is theoretical (Nix never emits that
/// many generations), but we still log and skip the state update rather
/// than panicking.
pub(super) fn record_attempt(
    state: &mut nmbl_init::state::State,
    generations: &[Generation],
    generation_index: usize,
    state_path: &Path,
) {
    let Some(target) = generations.get(generation_index) else {
        nmbl_warn!(
            "stateful: generation index {generation_index} out of range, skipping state.bin update",
        );
        return;
    };
    match nonmax::NonMaxU32::new(target.number) {
        Some(n) => state.last_attempted_generation = Some(n),
        None => {
            nmbl_warn!(
                "stateful: generation number {} is u32::MAX, cannot record in state.bin",
                target.number,
            );
            return;
        }
    }
    state.last_boot_succeeded = false;
    if let Err(err) = nmbl_init::state::write_padded(state_path, state) {
        nmbl_warn!(
            "stateful: failed to write state.bin at {}: {err}; proceeding with kexec anyway",
            state_path.display(),
        );
    }
}
