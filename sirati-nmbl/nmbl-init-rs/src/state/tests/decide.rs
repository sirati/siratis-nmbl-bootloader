#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::field_reassign_with_default,
    reason = "tests assert on contract failures"
)]

use nonmax::NonMaxU32;
use std::path::PathBuf;

use crate::generations::Generation;
use crate::state::{State, StatefulDecision, decide};

/// Build a synthetic [`Generation`] with just the `number` field
/// meaningful — `decide` only looks at `.number`, the rest is filler
/// the test never inspects.
fn fake_gen(n: u32) -> Generation {
    Generation {
        number: n,
        profile_link: PathBuf::from(format!("/profile-{n}")),
        toplevel: PathBuf::from(format!("/toplevel-{n}")),
        kernel: PathBuf::from("/kernel"),
        initrd: PathBuf::from("/initrd"),
        init_path: PathBuf::from("/init"),
        kernel_params: Vec::new(),
        label: String::new(),
    }
}

/// Convenience: build generations newest-first matching the order
/// `scan_generations` produces. Pass numbers in any order; this
/// re-sorts descending so callers can write `[10, 7, 3]` literally.
fn gens(numbers: &[u32]) -> Vec<Generation> {
    let mut v: Vec<Generation> = numbers.iter().map(|n| fake_gen(*n)).collect();
    v.sort_by_key(|g| std::cmp::Reverse(g.number));
    v
}

fn nm(n: u32) -> Option<NonMaxU32> {
    NonMaxU32::new(n)
}

#[test]
fn decide_success_inserts_new_into_known_good_front() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    state.last_attempted_generation = nm(42);
    state.known_good_generations[0] = nm(7);
    state.known_good_generations[1] = nm(5);
    state.recovery_attempt = 3;
    let gs = gens(&[42, 7, 5]);

    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::HonourTui);
    assert_eq!(state.known_good_generations[0], nm(42));
    assert_eq!(state.known_good_generations[1], nm(7));
    assert_eq!(state.known_good_generations[2], nm(5));
    // Tail drop: the previously-last slot is gone (was None anyway).
    assert_eq!(state.known_good_generations[19], None);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_success_inserts_drops_tail_when_full() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    state.last_attempted_generation = nm(100);
    for (i, slot) in state.known_good_generations.iter_mut().enumerate() {
        *slot = nm((i as u32) + 1);
    }
    // Active is gen 100 — present in generations; not in known_good.
    let mut numbers: Vec<u32> = (1..=20).collect();
    numbers.push(100);
    let gs = gens(&numbers);
    let active_index = gs.iter().position(|g| g.number == 100).expect("active");

    let _ = decide(&mut state, &gs, active_index, 3);
    assert_eq!(state.known_good_generations[0], nm(100));
    assert_eq!(state.known_good_generations[1], nm(1));
    // The original tail (slot 19 == 20) was dropped to make room.
    assert_eq!(state.known_good_generations[19], nm(19));
}

#[test]
fn decide_success_promotes_when_in_known_good_and_active() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    // The succeeded gen is the active one AND already in known_good
    // at index 2 — promote it to the front, shifting [0] and [1]
    // down by one.
    state.last_attempted_generation = nm(50);
    state.known_good_generations[0] = nm(99);
    state.known_good_generations[1] = nm(80);
    state.known_good_generations[2] = nm(50);
    state.recovery_attempt = 2;
    let gs = gens(&[99, 80, 50, 10]);
    let active_index = gs.iter().position(|g| g.number == 50).expect("active");

    let d = decide(&mut state, &gs, active_index, 3);
    assert_eq!(d, StatefulDecision::HonourTui);
    assert_eq!(state.known_good_generations[0], nm(50));
    assert_eq!(state.known_good_generations[1], nm(99));
    assert_eq!(state.known_good_generations[2], nm(80));
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_success_in_known_good_but_not_active_is_noop_on_array() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    state.last_attempted_generation = nm(50);
    state.known_good_generations[0] = nm(99);
    state.known_good_generations[2] = nm(50);
    state.recovery_attempt = 1;
    // Active is 99, not 50 — leave the array untouched.
    let gs = gens(&[99, 80, 50, 10]);
    let active_index = gs.iter().position(|g| g.number == 99).expect("active");
    let snapshot = state.known_good_generations;

    let d = decide(&mut state, &gs, active_index, 3);
    assert_eq!(d, StatefulDecision::HonourTui);
    assert_eq!(state.known_good_generations, snapshot);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_success_last_attempt_gc_is_noop_on_array() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    // 999 was attempted but is no longer on disk — array stays.
    state.last_attempted_generation = nm(999);
    state.known_good_generations[0] = nm(42);
    state.recovery_attempt = 1;
    let gs = gens(&[42, 7]);
    let snapshot = state.known_good_generations;

    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::HonourTui);
    assert_eq!(state.known_good_generations, snapshot);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_success_last_attempt_none_is_noop_on_array() {
    let mut state = State::default();
    state.last_boot_succeeded = true;
    state.last_attempted_generation = None;
    state.known_good_generations[0] = nm(42);
    state.recovery_attempt = 5;
    let gs = gens(&[42, 7]);
    let snapshot = state.known_good_generations;

    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::HonourTui);
    assert_eq!(state.known_good_generations, snapshot);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_failure_first_pick_from_known_good_slot_zero() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to exercise the failure path
    // — without it the fresh-state guard short-circuits to HonourTui.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    state.known_good_generations[0] = nm(50);
    let gs = gens(&[100, 50, 10]);
    let target = gs.iter().position(|g| g.number == 50).expect("target");

    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::ForcePick(target));
    assert_eq!(state.recovery_attempt, 1);
}

#[test]
fn decide_failure_first_pick_missing_falls_back_to_older() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state
    // guard; pin it at 100 (the active gen) so the fallback walk
    // still finds 50 below it.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    // Slot 0 is empty — must walk strictly OLDER gens. Active is
    // the newest (idx 0); fallback picks the next-older (idx 1).
    state.known_good_generations[0] = None;
    let gs = gens(&[100, 50, 10]);
    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::ForcePick(1));
    assert_eq!(state.recovery_attempt, 1);
}

#[test]
fn decide_failure_known_good_gc_falls_back_to_older() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state
    // guard — pin it at the active gen so the fallback still hits.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    // Points at gen 999 which is no longer on disk — first pick
    // misses, fallback walks older gens past `active_index`.
    state.known_good_generations[0] = nm(999);
    let gs = gens(&[100, 50, 10]);
    // Active = 100 (idx 0); fallback picks the next-older (idx 1 = 50).
    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::ForcePick(1));
    assert_eq!(state.recovery_attempt, 1);
}

#[test]
fn decide_failure_fallback_skips_known_good_entries() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state guard.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    // First pick miss — slot 0 absent.
    state.known_good_generations[0] = None;
    // But 50 IS marked known-good elsewhere in the ring — the
    // fallback walk must skip it and prefer 10 (the next older
    // entry strictly past active).
    state.known_good_generations[5] = nm(50);
    let gs = gens(&[100, 50, 10]);
    let active_index = gs.iter().position(|g| g.number == 100).expect("active");

    let d = decide(&mut state, &gs, active_index, 3);
    let target = gs.iter().position(|g| g.number == 10).expect("target");
    assert_eq!(d, StatefulDecision::ForcePick(target));
    assert_eq!(state.recovery_attempt, 1);
}

#[test]
fn decide_failure_fallback_skips_last_attempted_generation() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    state.recovery_attempt = 0;
    state.known_good_generations[0] = None;
    // We just tried 50 last boot — fallback must skip it and pick
    // 10 (the next strictly-older candidate).
    state.last_attempted_generation = nm(50);
    let gs = gens(&[100, 50, 10]);
    let active_index = gs.iter().position(|g| g.number == 100).expect("active");

    let d = decide(&mut state, &gs, active_index, 3);
    let target = gs.iter().position(|g| g.number == 10).expect("target");
    assert_eq!(d, StatefulDecision::ForcePick(target));
    assert_eq!(state.recovery_attempt, 1);
}

#[test]
fn decide_failure_exhausted_does_not_mutate_state() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    state.recovery_attempt = 3;
    state.last_attempted_generation = nm(42);
    state.known_good_generations[0] = nm(50);
    let gs = gens(&[100, 50, 10]);
    let snapshot = state.clone();

    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::Exhausted);
    assert_eq!(state, snapshot);
}

#[test]
fn decide_failure_no_candidate_returns_exhausted() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded — otherwise the fresh-state
    // guard short-circuits before the budget check.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    state.known_good_generations[0] = None;
    // active_index = 0 means no strictly older candidate; first
    // pick also misses — exhausted.
    let gs = gens(&[100]);
    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::Exhausted);
    // No mutation on the exhausted branch.
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_failure_empty_generations_returns_exhausted() {
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state guard.
    state.last_attempted_generation = nm(50);
    state.recovery_attempt = 0;
    state.known_good_generations[0] = nm(50);
    let gs: Vec<Generation> = Vec::new();
    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::Exhausted);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_failure_active_index_zero_does_not_pick_active() {
    // `generations` is sorted newest-first by `scan_generations`.
    // Active at index 0 (the newest) must still leave room for the
    // fallback to pick an older entry — and it must NOT return
    // index 0 itself. This pins the "older = higher index"
    // convention against accidental sign flips in the walk.
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state guard.
    state.last_attempted_generation = nm(100);
    state.recovery_attempt = 0;
    state.known_good_generations[0] = None;
    let gs = gens(&[100, 50]);
    let d = decide(&mut state, &gs, 0, 3);
    assert_eq!(d, StatefulDecision::ForcePick(1));
    assert_ne!(d, StatefulDecision::ForcePick(0));
}

#[test]
fn decide_failure_active_is_oldest_returns_exhausted() {
    // Active sits at the OLDEST slot (last index). No
    // strictly-older candidate exists, first pick missed, so the
    // result is Exhausted with no mutation.
    let mut state = State::default();
    state.last_boot_succeeded = false;
    // A prior attempt must be recorded to bypass the fresh-state guard.
    state.last_attempted_generation = nm(10);
    state.recovery_attempt = 0;
    state.known_good_generations[0] = None;
    let gs = gens(&[100, 50, 10]);
    let active_index = gs.iter().position(|g| g.number == 10).expect("active");

    let d = decide(&mut state, &gs, active_index, 3);
    assert_eq!(d, StatefulDecision::Exhausted);
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_first_boot_with_fresh_state_honours_tui() {
    // Regression: a freshly-initialised state.bin
    // (last_attempted_generation = None) on a single-generation
    // install must HonourTui rather than dropping to Exhausted.
    // Pre-fix this case routed straight to the emergency screen.
    let mut state = State::default();
    // Pin the failure branch explicitly: even with
    // `last_boot_succeeded = false`, the absence of a prior attempt
    // means there is nothing to roll back from.
    state.last_boot_succeeded = false;
    state.last_attempted_generation = None;
    let gs = gens(&[42]);
    let active_index = 0;
    let max_attempts = 5;

    let decision = decide(&mut state, &gs, active_index, max_attempts);
    assert_eq!(decision, StatefulDecision::HonourTui);
    // The fresh-state guard must not spend a recovery slot.
    assert_eq!(state.recovery_attempt, 0);
}

#[test]
fn decide_first_boot_default_state_honours_tui() {
    // The other half of the regression: `State::default()` now
    // initialises `last_boot_succeeded = true` (the "no failure
    // recorded yet" semantic). This pins that default so a future
    // refactor can't silently flip it back to `false` and resurrect
    // the emergency-screen loop.
    let mut state = State::default();
    let gs = gens(&[42]);
    let decision = decide(&mut state, &gs, 0, 5);
    assert_eq!(decision, StatefulDecision::HonourTui);
    assert!(state.last_boot_succeeded);
}
