//! Global event-loop tick counter driving the diagnostic spinner.
//!
//! A single process-wide counter, bumped exactly once per input-poll
//! cycle of the UI event loop (see [`crate::ui::console::LatchingConsole`],
//! the convergence point every interactive loop drives input through).
//! The top-right diagnostic spinner (drawn last in
//! [`crate::ui::screen_render::render_current_screen`]) maps this tick to
//! a glyph, so:
//!
//! * if the event loop keeps iterating, the counter advances and the
//!   spinner spins — visible proof the loop is alive;
//! * if a synchronous operation blocks the loop, no poll cycle runs, the
//!   counter freezes, and the spinner stops — that frozen glyph is the
//!   diagnostic signal.
//!
//! Because the frame is derived from this loop-driven counter rather than
//! a wall-clock timer or any per-screen state, the spinner is a faithful
//! liveness indicator for the loop itself, independent of any worker.
//!
//! ## Threading
//!
//! NMBL runs on a single-thread `LocalRuntime`. A plain thread-local
//! [`Cell`] is sufficient — no atomics, no `Send`/`Sync` bound — matching
//! the rest of the fork-safe single-thread runtime. Reads from another
//! thread (there are none) simply see their own zero-initialised cell.

use std::cell::Cell;

thread_local! {
    /// Monotonic count of event-loop input-poll cycles this session.
    /// Bumped by [`tick`], read by [`current`].
    static EVENT_LOOP_TICK: Cell<u64> = const { Cell::new(0) };
}

/// Advance the event-loop tick by one. Called once per input-poll cycle
/// from the single console convergence point so the count tracks loop
/// liveness, not wall-clock time. Wraps on overflow (harmless: the
/// spinner indexes modulo its frame count).
pub(crate) fn tick() {
    EVENT_LOOP_TICK.with(|c| c.set(c.get().wrapping_add(1)));
}

/// The current event-loop tick. The diagnostic spinner maps this to a
/// glyph via `current() % SPINNER_FRAMES`.
#[must_use]
pub(crate) fn current() -> u64 {
    EVENT_LOOP_TICK.with(Cell::get)
}

/// Map an event-loop tick to a spinner glyph index in `[0, frames)`.
///
/// Pulled out as a pure function so the tick→frame contract is unit
/// testable without a live event loop: tick `N` selects frame
/// `N % frames`.
#[must_use]
pub(crate) fn frame_index(tick: u64, frames: u8) -> usize {
    debug_assert!(frames > 0, "spinner must have at least one frame");
    let frames = u64::from(frames.max(1));
    (tick % frames) as usize
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, reason = "tests assert on contract bounds")]
mod tests {
    use super::{current, frame_index, tick};
    use crate::ui::app::{SPINNER_FRAMES, SPINNER_GLYPHS};

    #[test]
    fn tick_advances_by_one_per_call() {
        let before = current();
        tick();
        assert_eq!(current(), before + 1, "one tick = one increment");
        tick();
        tick();
        assert_eq!(current(), before + 3, "each call bumps exactly once");
    }

    #[test]
    fn frame_index_is_tick_modulo_frames() {
        // tick N → frame N % frames.len(), for a full cycle and beyond.
        let frames = SPINNER_FRAMES;
        for n in 0..(u64::from(frames) * 3 + 1) {
            assert_eq!(
                frame_index(n, frames),
                (n % u64::from(frames)) as usize,
                "tick {n} must map to frame {n} % {frames}"
            );
        }
    }

    #[test]
    fn frame_index_indexes_glyphs_in_bounds() {
        // The mapped index must always be a valid glyph slot, so the
        // overlay can index SPINNER_GLYPHS without bounds checks failing.
        for n in 0..1000u64 {
            let idx = frame_index(n, SPINNER_FRAMES);
            assert!(idx < SPINNER_GLYPHS.len(), "frame index out of glyph range");
            let _g: char = SPINNER_GLYPHS[idx];
        }
    }

    #[test]
    fn frame_index_handles_wrapping_tick() {
        // After wrap_add overflow the tick restarts at 0; the glyph cycle
        // must remain continuous (u64::MAX maps by plain modulo).
        let frames = SPINNER_FRAMES;
        assert_eq!(
            frame_index(u64::MAX, frames),
            (u64::MAX % u64::from(frames)) as usize
        );
    }
}
