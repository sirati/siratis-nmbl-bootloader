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
#[allow(
    clippy::indexing_slicing,
    reason = "ring slots are statically indexed within their fixed-size array; \
              the fallback `generations[idx]` is bounded by the `0..len()` range"
)]
pub fn decide(
    state: &mut State,
    generations: &[crate::generations::Generation],
    active_index: usize,
    max_recovery_attempts: u32,
) -> StatefulDecision {
    if state.last_boot_succeeded {
        rotate_known_good(state, generations, active_index);
        state.recovery_attempt = 0;
        return StatefulDecision::HonourTui;
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

    // First pick: try `known_good_generations[recovery_attempt]` if it
    // points at a generation that's still on disk.
    let r = state.recovery_attempt as usize;
    let mut picked: Option<usize> = None;
    if r < state.known_good_generations.len()
        && let Some(slot) = state.known_good_generations[r]
    {
        let n = slot.get();
        picked = generations.iter().position(|g| g.number == n);
    }

    if picked.is_none() {
        // Fallback walk: strictly OLDER than the active Nix profile.
        // `scan_generations` sorts newest-first (descending number), so
        // OLDER entries sit at HIGHER indices than `active_index`. Skip
        // anything already in known_good or the gen we tried most
        // recently (last_attempted_generation tracks the previous boot's
        // pick — preventing an immediate retry of the just-failed gen).
        // When `active_index` is already the oldest scanned generation,
        // the loop body never executes and we exhaust below.
        let last_attempt = state.last_attempted_generation.map(|v| v.get());
        picked = generations
            .iter()
            .enumerate()
            .skip(active_index + 1)
            .find_map(|(idx, g)| {
                let n = g.number;
                let in_known_good = state
                    .known_good_generations
                    .iter()
                    .any(|slot| slot.map(|v| v.get()) == Some(n));
                let is_last_attempt = last_attempt == Some(n);
                (!in_known_good && !is_last_attempt).then_some(idx)
            });
    }

    match picked {
        Some(idx) => {
            state.recovery_attempt = state.recovery_attempt.saturating_add(1);
            StatefulDecision::ForcePick(idx)
        }
        None => StatefulDecision::Exhausted,
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
