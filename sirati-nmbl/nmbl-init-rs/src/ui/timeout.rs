//! Countdown loop with first-key cancellation.
//!
//! This module implements the boot-menu auto-select timer described in
//! `PLAN.md` §7 (phase 4 TUI) / §8 (user interaction): we block for at
//! most `duration`, ticking a render callback once a second, and return
//! [`TimeoutOutcome::Cancelled`] the instant the user presses any key.
//!
//! Terminal lifecycle: this function does **not** enter or leave raw
//! mode. The caller (the surrounding `ui` orchestrator) is responsible
//! for holding a live [`crate::sys::tty::RawModeGuard`] across the call.
//! Without raw mode, `crossterm::event::poll` will still return, but
//! the kernel line-discipline will buffer keystrokes until a newline,
//! defeating the "first key cancels" contract.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};

use crate::error::{NmblError, Result};
use crate::ui::POLL_SLICE;

/// Outcome of running the countdown loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutOutcome {
    /// Timer expired with no input.
    Expired,
    /// User pressed a key (cancels the countdown).
    Cancelled,
}

/// Block for at most `duration`, polling stdin for any key press.
///
/// Each second elapsed, `on_tick(remaining_secs)` is invoked with the
/// floor of the remaining duration so the caller can update its render.
/// `on_tick` is called once immediately with the initial second so the
/// first frame is correct.
///
/// Returns [`TimeoutOutcome::Cancelled`] on the first `KeyEventKind::Press`
/// event, [`TimeoutOutcome::Expired`] when the deadline passes.
///
/// Errors from `crossterm::event::poll` / `crossterm::event::read` are
/// mapped to [`NmblError::Tui`]; crossterm 0.28 returns `std::io::Error`
/// directly from both calls.
///
/// # Preconditions
/// The caller must already hold a live
/// [`crate::sys::tty::RawModeGuard`]; otherwise keystrokes will be
/// line-buffered by the kernel and cancellation will only fire on Enter.
pub fn run_countdown<F: FnMut(u64)>(duration: Duration, mut on_tick: F) -> Result<TimeoutOutcome> {
    let start = Instant::now();
    let deadline = start.checked_add(duration).unwrap_or(start);

    // Emit the initial frame so the caller can paint "Booting in N
    // seconds" before we sleep. A zero-duration call returns Expired
    // here without ever polling.
    let initial = duration.as_secs();
    on_tick(initial);
    let mut last_reported = initial;

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };

        let slice = remaining.min(POLL_SLICE);
        match event::poll(slice).map_err(tui_err)? {
            true => {
                // Drain exactly one event so a stray Release doesn't
                // leave us spinning on the same Press indefinitely.
                let evt = event::read().map_err(tui_err)?;
                if let Event::Key(key) = evt
                    && key.kind == KeyEventKind::Press
                {
                    return Ok(TimeoutOutcome::Cancelled);
                }
                // Non-Press events (Release, Repeat, resize, mouse,
                // paste, focus) fall through and keep the countdown
                // running.
            }
            false => {
                // No input arrived in this slice. Recompute remaining
                // and tick the renderer if the displayed second
                // changed.
                let now = Instant::now();
                let Some(remaining) = deadline.checked_duration_since(now) else {
                    return Ok(TimeoutOutcome::Expired);
                };
                let secs = remaining.as_secs();
                if secs != last_reported {
                    on_tick(secs);
                    last_reported = secs;
                }
            }
        }
    }
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test assertions")]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn zero_duration_expires_immediately_with_one_tick() {
        // The initial on_tick fires before we ever poll, so a
        // zero-duration call must return Expired having reported 0
        // remaining seconds exactly once.
        let ticks = Cell::new(0u32);
        let last = Cell::new(u64::MAX);
        let outcome = run_countdown(Duration::ZERO, |remaining| {
            ticks.set(ticks.get() + 1);
            last.set(remaining);
        });
        assert!(matches!(outcome, Ok(TimeoutOutcome::Expired)));
        assert_eq!(ticks.get(), 1);
        assert_eq!(last.get(), 0);
    }

    #[test]
    fn sub_second_duration_expires_with_single_tick() {
        // 1 ms is below the 100 ms poll slice and well under one
        // second, so we expect the initial tick and then expiry. In
        // CI we have no controlling tty; `crossterm::event::poll` is
        // documented to error in that case, so we accept either
        // Expired or Tui-mapped failure as long as the on_tick contract
        // (exactly one call for the initial frame) held.
        let ticks = Cell::new(0u32);
        let outcome = run_countdown(Duration::from_millis(1), |_| {
            ticks.set(ticks.get() + 1);
        });
        match outcome {
            Ok(TimeoutOutcome::Expired) => {}
            Err(NmblError::Tui { .. }) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(ticks.get(), 1);
    }
}
