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

    // 1. Countdown phase. `timeout_ms` is the auto-boot budget in
    //    milliseconds; sub-second values are supported and the display
    //    rounds up so it never reads a misleading "0s".
    let countdown = Duration::from_millis(u64::from(config.general.timeout_ms));
    // Arm the auto-boot countdown ONLY on a fully unattended boot. If the
    // operator has pressed any key this session (e.g. a LUKS passphrase
    // before the menu) the shared latch is set, so we skip the countdown
    // entirely and fall through to the event loop to wait indefinitely
    // for an explicit choice — the same presence gate the emergency
    // screen applies before arming its auto-reboot timer.
    let outcome = if app.interaction.get() {
        app.countdown_remaining_secs = None;
        TimeoutOutcome::Cancelled
    } else {
        run_console_countdown(console, &mut app, countdown).await?
    };
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
            // `UserHasInteracted` only matters during the countdown phase
            // (already past here); in the event loop the real key that
            // follows it does the work, so the notice is a no-op.
            Some(
                crate::ui::console::ConsoleEvent::Scroll { .. }
                | crate::ui::console::ConsoleEvent::UserHasInteracted,
            )
            | None => {}
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
        // Cancel the countdown on the central layer's one-shot
        // `UserHasInteracted` — the single source of "operator is
        // present". The wrapper already set the shared latch when it
        // emitted this, so a later screen this session sees the boot as
        // attended without the selector touching the latch itself. A
        // `Resize` only repaints (the selector loop below redraws at the
        // new geometry) and a raw `Key` is delivered *after* the notice,
        // so neither needs to cancel here.
        if let Some(crate::ui::console::ConsoleEvent::UserHasInteracted) =
            console.poll_event(slice).await?
        {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Decision, run_console_countdown, run_selector_on_console};
    use crate::config::Config;
    use crate::error::Result;
    use crate::generations::Generation;
    use crate::ui::app::{App, SessionInteraction};
    use crate::ui::console::{Console, ConsoleEvent, ConsoleKind, LatchingConsole};
    use crate::ui::timeout::TimeoutOutcome;

    /// Console double that replays canned key events; once drained it
    /// yields `None` so the countdown loop relies on its own deadline.
    struct ScriptedConsole {
        keys: VecDeque<KeyEvent>,
    }

    impl ScriptedConsole {
        fn new(keys: Vec<KeyEvent>) -> Self {
            Self { keys: keys.into() }
        }
    }

    impl Console for ScriptedConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            Ok(())
        }
        fn poll_event<'a>(
            &'a mut self,
            timeout: std::time::Duration,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
            Box::pin(async move { self.poll_event_blocking(timeout) })
        }
        fn poll_event_blocking(
            &mut self,
            _timeout: std::time::Duration,
        ) -> Result<Option<ConsoleEvent>> {
            Ok(self.keys.pop_front().map(ConsoleEvent::Key))
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn block<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("test runtime");
        rt.block_on(fut)
    }

    fn sel_gen(number: u32) -> Generation {
        Generation {
            number,
            profile_link: std::path::PathBuf::from(format!("/p/system-{number}-link")),
            kernel: std::path::PathBuf::from("/p/kernel"),
            initrd: std::path::PathBuf::from("/p/initrd"),
            init_path: std::path::PathBuf::from(format!("/p/system-{number}-link/init")),
            kernel_params: Vec::new(),
            label: String::new(),
        }
    }

    #[test]
    fn selector_skips_auto_boot_when_session_already_interacted() {
        // Attended boot: the operator pressed a key earlier this session
        // (e.g. a LUKS passphrase) so the shared latch is already set.
        // The selector must NOT arm the auto-boot countdown and must NOT
        // boot the default on timeout — it waits for an explicit choice.
        let cfg: Config = toml::from_str("").expect("default cfg");
        let gens = vec![sel_gen(2), sel_gen(1)];
        let session = SessionInteraction::new();
        session.set();

        // Default highlight is index 0. Move down then confirm: an Enter
        // on index 1. A timeout auto-boot would instead emit index 0, so
        // the booted index proves the choice came from the event loop and
        // not the (skipped) countdown.
        let mut console = ScriptedConsole::new(vec![press(KeyCode::Down), press(KeyCode::Enter)]);
        let decision = block(run_selector_on_console(
            &cfg,
            &gens,
            &mut console,
            0,
            &session,
        ))
        .expect("selector returns the explicit choice");
        match decision {
            Decision::Boot {
                generation_index,
                cmdline_override,
            } => {
                assert_eq!(
                    generation_index, 1,
                    "must boot the operator's explicit choice, not the timeout default (index 0)"
                );
                assert!(cmdline_override.is_none());
            }
            other => panic!("expected explicit Boot decision, got {other:?}"),
        }
    }

    #[test]
    fn countdown_cancel_records_operator_presence() {
        // A keypress during the countdown is operator presence. The
        // central `LatchingConsole` latches it and emits
        // `UserHasInteracted`; the countdown cancels on that notice (NOT
        // on the raw key) and the shared latch is set so later screens
        // this session treat the boot as attended.
        let gens = vec![sel_gen(1)];
        let session = SessionInteraction::new();
        let mut app = App::new_in_session(&gens, &session);
        assert!(!session.get());

        let scripted = ScriptedConsole::new(vec![press(KeyCode::Char('x'))]);
        let mut console = LatchingConsole::new(Box::new(scripted), session.clone());
        let outcome = block(run_console_countdown(
            &mut console,
            &mut app,
            std::time::Duration::from_secs(60),
        ))
        .expect("countdown polls the console");
        assert_eq!(outcome, TimeoutOutcome::Cancelled);
        assert!(
            session.get(),
            "cancelling the countdown must latch operator presence"
        );
    }
}
