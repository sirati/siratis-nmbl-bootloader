//! Emergency event-loop driver.
//!
//! `drive_emergency_loop` renders the emergency screen, polls events,
//! reacts to keypresses, and enforces the auto-reboot countdown.
//! `now` is injected so unit tests can manipulate the clock without
//! sleeping real wall-clock seconds.

use std::time::{Duration, Instant};

use crate::error::{NmblError, Result};
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, EmergencyChoice, Screen};
use crate::ui::console::{Console, ConsoleEvent};

/// Arm the countdown deadline (if the boot is unattended) and compute
/// the initial displayed seconds.
///
/// Returns `Ok(initial_secs)` when the loop should start, or
/// `Err(EmergencyChoice::Reboot)` when the deadline is already in the
/// past and the loop should exit immediately.
fn init_countdown<N>(app: &mut App<'_>, now: &N) -> std::result::Result<u64, EmergencyChoice>
where
    N: Fn() -> Instant,
{
    // Mirror the deadline into the App's display field. Only set the
    // displayed remaining-seconds if the deadline is armed; a session
    // that the operator already touched has `error_countdown_deadline
    // == None` and shows no countdown.
    match app.error_countdown_deadline {
        Some(d) => match d.checked_duration_since(now()) {
            Some(r) => {
                let s = r.as_secs();
                app.countdown_remaining_secs = Some(s);
                Ok(s)
            }
            None => {
                // Deadline already in the past on entry — reboot
                // immediately. Matches the spec's "past_instant" case.
                Err(EmergencyChoice::Reboot)
            }
        },
        None => {
            // No deadline armed — the countdown UI stays hidden and
            // the loop only exits on keypress.
            app.countdown_remaining_secs = None;
            Ok(0)
        }
    }
}

/// Tick the displayed countdown if the visible second has changed.
///
/// Called on every empty-poll iteration. Skipped (no-op) when the
/// deadline is disarmed — the operator has already touched the menu.
/// Returns `Err(Reboot)` when the deadline has elapsed during the tick,
/// otherwise returns whether the display changed and needs a repaint.
fn tick_countdown_display<N>(
    app: &mut App<'_>,
    last_reported: &mut u64,
    now: &N,
) -> std::result::Result<bool, EmergencyChoice>
where
    N: Fn() -> Instant,
{
    let Some(d) = app.error_countdown_deadline else {
        return Ok(false);
    };
    let Some(remaining) = d.checked_duration_since(now()) else {
        return Err(EmergencyChoice::Reboot);
    };
    let secs = remaining.as_secs();
    if secs != *last_reported {
        app.countdown_remaining_secs = Some(secs);
        *last_reported = secs;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Shared event-loop driver. Render, poll, react, repeat — and apply
/// the no-input timeout. Returns the operator's choice or the default
/// when the timer expires.
///
/// `now` is injected so tests can drive the timeout machinery without
/// real wall-clock waits.
pub(crate) async fn drive_emergency_loop<N>(
    app: &mut App<'_>,
    timeout: Duration,
    now: N,
    console: &mut dyn Console,
) -> Result<EmergencyChoice>
where
    N: Fn() -> Instant,
{
    // Arm the auto-reboot countdown ONLY on a fully unattended boot. If
    // the operator has pressed any key this session (boot menu, LUKS
    // passphrase, a prior visit to this screen) they are present, so we
    // leave the deadline disarmed and wait indefinitely for a choice.
    // The latch itself is a no-op when a deadline is already `Some(_)`,
    // so a still-running loop's keypress-cleared deadline is never
    // re-armed here (the latch lives at function entry only).
    if !app.interaction.get() {
        app.latch_error_countdown(timeout);
    }

    let mut last_reported = match init_countdown(app, &now) {
        Ok(s) => s,
        Err(choice) => return Ok(choice),
    };

    let mut dirty = true;
    loop {
        if dirty {
            console.render(app)?;
            dirty = false;
        }

        // Resolve the poll slice against the (possibly disarmed)
        // deadline. With no deadline we poll on the unconditional
        // slice and never time out.
        let slice = match app.error_countdown_deadline {
            Some(d) => match d.checked_duration_since(now()) {
                Some(r) => r.min(POLL_SLICE),
                None => return Ok(EmergencyChoice::Reboot),
            },
            None => POLL_SLICE,
        };

        // Race the input poll against the latched-deadline slice. `biased`
        // polls input first so a keypress that lands exactly on the slice
        // boundary still cancels the countdown rather than racing the timer.
        // We use `poll_event` (not `poll_key`) so a resize repaints immediately.
        let event = tokio::select! {
            biased;
            ev = console.poll_event(slice) => ev?,
            () = tokio::time::sleep(slice) => None,
        };
        match event {
            Some(ConsoleEvent::Resize { .. }) => {
                dirty = true;
                continue;
            }
            Some(ConsoleEvent::Key(key)) => {
                // Any keypress cancels the auto-reboot countdown for the
                // remainder of this session: clear both the display field
                // and the latched deadline so re-entries don't re-arm it.
                app.countdown_remaining_secs = None;
                app.error_countdown_deadline = None;
                if app.on_key(key) {
                    break;
                }
                dirty = true;
                continue;
            }
            None => {}
        }

        // No input this slice — tick the countdown display if warranted.
        match tick_countdown_display(app, &mut last_reported, &now) {
            Ok(changed) => dirty |= changed,
            Err(choice) => return Ok(choice),
        }
    }

    match &app.screen {
        Screen::Emergency { chosen, .. } => Ok(chosen.unwrap_or(EmergencyChoice::Reboot)),
        _ => Err(NmblError::Tui {
            source: std::io::Error::other("emergency screen exited off-screen"),
        }),
    }
}
