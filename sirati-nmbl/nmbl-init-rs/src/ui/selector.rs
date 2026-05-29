//! Boot-generation selector TUI — event loop and countdown driver.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::{Generation, active_generation_index};
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, Decision, SessionInteraction};
use crate::ui::console::Console;
use crate::ui::timeout::TimeoutOutcome;

/// Run the boot-selection TUI on the provided [`Console`] and return
/// the operator's decision.
///
/// The console is brought up once by the orchestrator (main.rs) at the
/// start of phase 1 and held through every phase; this function reuses
/// it instead of opening a parallel splash bring-up, so the same DRM
/// card / raw-mode tty serves the whole boot. Serial UARTs go through
/// the same path — the TUI's crossterm backend emits portable
/// vt100/xterm escapes that every modern serial terminal renders.
pub async fn run_selector(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
    session: &SessionInteraction,
) -> Result<Decision> {
    // The pre-selected entry must match the active `system` profile so
    // an operator who ran `nixos-rebuild --rollback` sees (and on
    // timeout boots) the generation they rolled back to — not the
    // higher-numbered one they rolled away from.
    let default_index = active_generation_index(generations, &config.paths.nix_profiles_dir);
    run_selector_on_console(config, generations, console, default_index, session).await
}

/// TUI event loop. Backend-agnostic: every render and key-poll goes
/// through the [`Console`] trait. Hosts the countdown, the List/Editing
/// state machine, and the timeout-defaults-to-active-profile rule.
async fn run_selector_on_console(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
    default_index: usize,
    session: &SessionInteraction,
) -> Result<Decision> {
    let mut app = App::new_in_session(generations, session);
    app.selected_index = default_index;
    app.show_kernel_params = config.tui.show_kernel_params;

    // 1. Countdown phase. A `timeout_ms` override (when set) wins over
    //    the whole-second `timeout_secs`, enabling sub-second auto-boot
    //    delays; absent it falls back to the historic seconds budget.
    let countdown = match config.general.timeout_ms {
        Some(ms) => Duration::from_millis(u64::from(ms)),
        None => Duration::from_secs(u64::from(config.general.timeout_secs)),
    };
    let outcome = run_console_countdown(console, &mut app, countdown).await?;
    app.countdown_remaining_secs = None;

    if matches!(outcome, TimeoutOutcome::Expired) && app.decision.is_none() {
        // Countdown reached zero without input — boot the same entry
        // the list was highlighting (the active profile).
        return Ok(Decision::Boot {
            generation_index: default_index,
            cmdline_override: None,
        });
    }

    // 2. Event loop. Renders on dirty, polls in short slices so future
    //    callers that need to drive an animation can plug in without
    //    rewriting the loop. Driven via `poll_event` so a host-reported
    //    `CSI 8;rows;cols t` resize redraws the picker against the new
    //    grid instead of stranding the old layout.
    let mut dirty = true;
    loop {
        if dirty {
            console.render(&app)?;
            dirty = false;
        }
        match console.poll_event(POLL_SLICE).await? {
            Some(crate::ui::console::ConsoleEvent::Resize { .. }) => {
                dirty = true;
            }
            Some(crate::ui::console::ConsoleEvent::Key(key)) => {
                if app.on_key(key) {
                    break;
                }
                dirty = true;
            }
            // No scrollback on the selector screen; ignore wheel notches.
            Some(crate::ui::console::ConsoleEvent::Scroll { .. }) | None => {}
        }
        if app.decision.is_some() {
            break;
        }
    }

    app.decision.ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other("selector exited without decision"),
    })
}

/// Remaining whole seconds, rounded UP, for the countdown header.
/// A non-zero sub-second remainder still reads as at least "1s" so a
/// sub-second `timeout_ms` never displays a misleading "0s".
fn ceil_secs(d: Duration) -> u64 {
    let secs = d.as_secs();
    if d.subsec_nanos() > 0 {
        secs.saturating_add(1)
    } else {
        secs
    }
}

/// Countdown driver that polls the [`Console`] for keys instead of
/// stdin, so cancel-on-keypress works on both the splash framebuffer
/// (input via `/dev/tty1`) and the raw-mode tty.
async fn run_console_countdown(
    console: &mut dyn Console,
    app: &mut App<'_>,
    duration: Duration,
) -> Result<TimeoutOutcome> {
    let start = Instant::now();
    let deadline = start.checked_add(duration).unwrap_or(start);

    // Round the displayed remaining time UP so a sub-second budget
    // (e.g. a 500 ms `timeout_ms`) shows "1s" rather than a misleading
    // "0s". Whole-second budgets are unaffected.
    let initial = ceil_secs(duration);
    app.countdown_remaining_secs = Some(initial);
    console.render(app)?;
    let mut last_reported = initial;

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };

        let slice = remaining.min(POLL_SLICE);
        // Any key cancels the countdown. A `Resize` only repaints (the
        // selector loop below redraws at the new geometry); it does not
        // count as the operator cancelling, matching the prior
        // `poll_key` semantics which silently dropped resizes.
        if let Some(crate::ui::console::ConsoleEvent::Key(_)) = console.poll_event(slice).await? {
            return Ok(TimeoutOutcome::Cancelled);
        }

        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };
        let secs = ceil_secs(remaining);
        if secs != last_reported {
            app.countdown_remaining_secs = Some(secs);
            console.render(app)?;
            last_reported = secs;
        }
    }
}
