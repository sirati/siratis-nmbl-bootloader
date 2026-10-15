use super::types::{State, StatefulDecision};

/// Run the rollback decision for the current boot.
///
/// `decide` is called ONCE per boot, BEFORE the kexec dispatch, with:
/// - `state`: the on-disk `state.bin` decoded into memory. Mutated in
///   place per the rules below; the caller writes it back to disk on
///   the non-Exhausted branches.
/// - `generations`: the result of `scan_generations`, sorted newest-first.
/// - `active_index`: the index inside `generations` of the currently
///   active Nix system profile, as returned by `active_generation_index`.
/// - `max_recovery_attempts`: operator-configured rollback budget.
///
/// `decide` does NOT touch `last_attempted_generation` or
/// `last_boot_succeeded` — that bookkeeping belongs to the caller, which
/// records the chosen generation and clears the success flag immediately
/// before kexec. Keeping those writes outside `decide` makes the
/// rotation/reset semantics independent of where the resulting boot ends
/// up going.
///
/// An unproven generation (installed, never booted successfully) is never
/// a rollback target. Once it has failed a boot, a healthy boot keeps
/// force-picking the newest known-good generation while the profile still
/// names it, without spending recovery budget.
pub fn decide(
    state: &mut State,
    generations: &[crate::generations::Generation],
    active_index: usize,
    max_recovery_attempts: u32,
) -> StatefulDecision {
    if state.last_boot_succeeded {
        rotate_known_good(state, generations, active_index);
        state.recovery_attempt = 0;
        return avoid_failed_unproven(state, generations, active_index);
    }

    // Failure of the single rescue-exit retry returns directly to rescue.
    if state.rescue_exit_retry_in_progress {
        return StatefulDecision::Exhausted;
    }

    // First boot with a fresh state.bin: no prior attempt was recorded,
    // so there is nothing to roll back from. Honour the TUI/timeout pick
    // rather than spending a recovery slot before any failure happens.
    // Belt-and-braces with the `Default::default()` change that sets
    // `last_boot_succeeded = true`; this also covers hand-rolled or
    // version-skewed States that arrive with both fields cleared.
    if state.last_attempted_generation.is_none() {
        return StatefulDecision::HonourTui;
    }

    // Failure path. Budget check first — never mutate state if we're
    // already over budget, the caller may decide to skip the write.
    if state.recovery_attempt >= max_recovery_attempts {
        return StatefulDecision::Exhausted;
    }

    // The failed boot was the unproven generation's: remember it, so later
    // healthy boots do not walk straight back into it.
    if state.unproven_generation.is_some()
        && state.last_attempted_generation == state.unproven_generation
    {
        state.unproven_failed = true;
    }

    match pick_fallback(state, generations, active_index) {
        Some(idx) => {
            state.recovery_attempt = state.recovery_attempt.saturating_add(1);
            StatefulDecision::ForcePick(idx)
        }
        None => StatefulDecision::Exhausted,
    }
}

/// Choose the rollback target for a failed boot: the known-good ring slot
/// for this attempt when it is still on disk, otherwise the newest
/// generation older than the active profile that is neither known-good nor
/// the one that just failed. The unproven generation is never chosen.
fn pick_fallback(
    state: &State,
    generations: &[crate::generations::Generation],
    active_index: usize,
) -> Option<usize> {
    let unproven = state.unproven_generation.map(|v| v.get());
    let r = state.recovery_attempt as usize;
    let ring_pick = state
        .known_good_generations
        .get(r)
        .copied()
        .flatten()
        .map(|slot| slot.get())
        .filter(|n| Some(*n) != unproven)
        .and_then(|n| generations.iter().position(|g| g.number == n));
    if ring_pick.is_some() {
        return ring_pick;
    }

    // Fallback walk: strictly OLDER than the active Nix profile.
    // `scan_generations` sorts newest-first (descending number), so
    // OLDER entries sit at HIGHER indices than `active_index`. Skip
    // anything already in known_good or the gen we tried most
    // recently (last_attempted_generation tracks the previous boot's
    // pick — preventing an immediate retry of the just-failed gen).
    // When `active_index` is already the oldest scanned generation,
    // the walk finds nothing and the caller exhausts.
    let last_attempt = state.last_attempted_generation.map(|v| v.get());
    generations
        .iter()
        .enumerate()
        .skip(active_index.saturating_add(1))
        .find_map(|(idx, g)| {
            let n = g.number;
            let in_known_good = state
                .known_good_generations
                .iter()
                .any(|slot| slot.map(|v| v.get()) == Some(n));
            let skip = in_known_good || last_attempt == Some(n) || unproven == Some(n);
            (!skip).then_some(idx)
        })
}

/// Healthy-boot guard for an unproven generation that already failed: while
/// the active profile still names it, boot the newest known-good generation
/// on disk instead. No recovery budget is spent; the operator retries the
/// unproven generation deliberately with `nmblctl retry-generation`.
fn avoid_failed_unproven(
    state: &State,
    generations: &[crate::generations::Generation],
    active_index: usize,
) -> StatefulDecision {
    let Some(unproven) = state
        .unproven_generation
        .map(|v| v.get())
        .filter(|_| state.unproven_failed)
    else {
        return StatefulDecision::HonourTui;
    };
    if generations.get(active_index).map(|g| g.number) != Some(unproven) {
        return StatefulDecision::HonourTui;
    }
    let fallback = state
        .known_good_generations
        .iter()
        .flatten()
        .map(|slot| slot.get())
        .filter(|n| *n != unproven)
        .find_map(|n| {
            generations
                .iter()
                .position(|g| g.number == n)
                .map(|i| (n, i))
        });
    match fallback {
        Some((known_good, idx)) => {
            crate::nmbl_info!(
                "stateful: generation {unproven} is unproven and failed its last boot; \
                 booting known-good {known_good}; use nmblctl retry-generation to retry it"
            );
            StatefulDecision::ForcePick(idx)
        }
        None => {
            crate::nmbl_warn!(
                "stateful: generation {unproven} is unproven and failed its last boot, \
                 but no known-good generation is left on disk; honouring the selector"
            );
            StatefulDecision::HonourTui
        }
    }
}

/// Rotate the known-good ring after a successful boot.
///
/// Extracted from `decide` to keep that function under the 100-line limit.
#[allow(
    clippy::indexing_slicing,
    reason = "ring slots are statically indexed within their fixed-size array"
)]
fn rotate_known_good(
    state: &mut State,
    generations: &[crate::generations::Generation],
    active_index: usize,
) {
    if let Some(last) = state.last_attempted_generation {
        let n = last.get();
        // Only rotate if the gen is still on disk — a GC'd target
        // is treated as if we never attempted it. The array stays
        // a snapshot of generations actually available right now.
        if generations.iter().any(|g| g.number == n) {
            let existing = state
                .known_good_generations
                .iter()
                .position(|slot| slot.map(|v| v.get()) == Some(n));
            match existing {
                None => {
                    // Shift right by one, drop the tail, insert at [0].
                    let len = state.known_good_generations.len();
                    for i in (1..len).rev() {
                        state.known_good_generations[i] = state.known_good_generations[i - 1];
                    }
                    state.known_good_generations[0] = Some(last);
                }
                Some(pos) => {
                    // Only re-promote to the front when the
                    // succeeded gen is the one Nix considers active
                    // — otherwise the operator just rolled forward
                    // past a known-good and the ring already
                    // captured that boot at the right place.
                    let active_n = generations.get(active_index).map(|g| g.number);
                    if Some(n) == active_n && pos > 0 {
                        let slot = state.known_good_generations[pos];
                        for i in (1..=pos).rev() {
                            state.known_good_generations[i] = state.known_good_generations[i - 1];
                        }
                        state.known_good_generations[0] = slot;
                    }
                }
            }
        }
    }
}

/// Record an explicit operator-authorized attempt without changing automatic
/// recovery budget, known-good history, or manufacturing a health blessing.
/// The caller consumes one-use authorization and persists this before kexec.
pub fn record_operator_retry(
    state: &mut State,
    generations: &[crate::generations::Generation],
    number: u32,
) -> Option<usize> {
    let generation = nonmax::NonMaxU32::new(number).filter(|_| number > 0)?;
    let index = generations.iter().position(|g| g.number == number)?;
    state.rescue_booted_generation = None;
    state.rescue_exit_retry_in_progress = false;
    state.last_attempted_generation = Some(generation);
    state.last_boot_succeeded = false;
    Some(index)
}

/// Whether the active profile generation (`None` when the profile could not
/// be read) may be the unproven one. Instant boot must then show the menu;
/// an unreadable profile is treated as unproven while a marker exists.
#[must_use]
pub fn active_is_unproven(state: &State, active_profile: Option<u32>) -> bool {
    state
        .unproven_generation
        .is_some_and(|unproven| active_profile.is_none_or(|n| n == unproven.get()))
}
