//! Boot-generation selector TUI — event loop and countdown driver.

use std::time::{Duration, Instant};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::{Generation, active_generation_index};
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, Decision, SessionInteraction};
use crate::ui::console::Console;
use crate::ui::countdown::{CountdownAction, CountdownOutcome, CountdownScreen};

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
pub(crate) async fn run_selector_on_console(
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
        CountdownOutcome::Cancelled
    } else {
        run_console_countdown(console, &mut app, countdown).await?
    };
    app.countdown_remaining_secs = None;

    if matches!(outcome, CountdownOutcome::Expired) && app.decision.is_none() {
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
    //
    //    Input is *coalesced*: once the blocking poll surfaces an event we
    //    keep draining every already-ready event with a zero timeout and
    //    apply each to the App *before* a single redraw. A burst or a held
    //    arrow key (the log viewer's worst case — holding Down through a
    //    multi-thousand-line kernel log) thus collapses to ONE frame at the
    //    final offset instead of one full-screen redraw + framebuffer flush
    //    per keystroke. The `dirty` flag still skips the redraw entirely
    //    when nothing changed state, so we never blit an unchanged frame.
    //
    //    On TOP of dirty: a 30 fps cap (`FRAME_INTERVAL`). A frame paints
    //    only when `dirty` AND at least `FRAME_INTERVAL` has elapsed since
    //    the last render, so a fast input burst can dirty the App many
    //    times but repaints no more than 30×/s. When a change is pending
    //    but the interval has not elapsed, the first poll's wait is bounded
    //    to the time left until the next allowed render, so the loop wakes
    //    by that deadline and the final state paints within ~33 ms — the
    //    last frame after a burst is never dropped.
    let mut dirty = true;
    // Seed so the very first pass renders immediately (interval already
    // satisfied) instead of waiting out a frame interval before the
    // initial paint.
    let mut last_render = Instant::now()
        .checked_sub(FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);
    loop {
        let gate = render_gate(dirty, last_render, Instant::now());
        if gate.render {
            console.render(&app)?;
            last_render = Instant::now();
            dirty = false;
        }
        // Block for the first event, bounded by the frame-render deadline
        // when a paint is pending, then drain the rest of the ready burst
        // non-blockingly so they all land in one frame.
        let mut timeout = gate.wait;
        loop {
            match console.poll_event(timeout).await? {
                Some(crate::ui::console::ConsoleEvent::Resize { .. }) => {
                    dirty = true;
                }
                Some(crate::ui::console::ConsoleEvent::Key(key)) => {
                    if app.on_key(key) {
                        // A Decision (or screen pop) committed; stop
                        // draining and let the outer loop notice.
                        break;
                    }
                    dirty = true;
                }
                // No scrollback on the selector screen; ignore wheel
                // notches. `UserHasInteracted` only matters during the
                // countdown phase (already past here); in the event loop
                // the real key that follows it does the work, so the
                // notice is a no-op.
                Some(
                    crate::ui::console::ConsoleEvent::Scroll { .. }
                    | crate::ui::console::ConsoleEvent::UserHasInteracted,
                ) => {}
                // No more events ready right now — break out and redraw
                // once if anything changed.
                None => break,
            }
            if app.decision.is_some() {
                break;
            }
            // Subsequent drains must not block: only fold in events that
            // are already queued so we don't stall waiting for more input.
            timeout = std::time::Duration::ZERO;
        }
        if app.decision.is_some() {
            break;
        }
    }

    app.decision.ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other("selector exited without decision"),
    })
}

/// Minimum wall-time between two consecutive screen renders: a 30 fps
/// cap (~33.33 ms). Composed with the `dirty` flag so the event loop
/// renders at most 30 times a second AND only when state changed.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Decision of the frame-rate gate for one event-loop pass.
struct RenderGate {
    /// Whether a frame should be painted this pass.
    render: bool,
    /// Upper bound for the *first* `poll_event` wait this pass. When a
    /// change is pending but the frame interval has not elapsed yet, this
    /// shrinks below `POLL_SLICE` to the time left until the next allowed
    /// render, so the loop wakes by that deadline and paints the pending
    /// frame even if no further input arrives. Otherwise it is `POLL_SLICE`.
    wait: Duration,
}

/// Compose the 30 fps cap with the `dirty` flag for a single loop pass.
///
/// `now` is passed in (rather than read from the clock here) so the gate
/// is a pure function the tests drive against a controlled monotonic
/// timeline without real sleeps. `last_render` is the [`Instant`] the
/// previous frame was painted at.
///
/// * not dirty            ⇒ never render; full `POLL_SLICE` wait.
/// * dirty, interval up   ⇒ render this pass; full `POLL_SLICE` wait.
/// * dirty, interval left ⇒ don't render yet, but bound the wait to the
///   remaining time so the deadline wakes us to paint the final frame.
fn render_gate(dirty: bool, last_render: Instant, now: Instant) -> RenderGate {
    if !dirty {
        return RenderGate {
            render: false,
            wait: POLL_SLICE,
        };
    }
    let elapsed = now.duration_since(last_render);
    if elapsed >= FRAME_INTERVAL {
        RenderGate {
            render: true,
            wait: POLL_SLICE,
        }
    } else {
        // Wake by the next-render deadline so the pending change still
        // paints once the interval passes, even with no new input.
        let remaining = FRAME_INTERVAL - elapsed;
        RenderGate {
            render: false,
            wait: POLL_SLICE.min(remaining),
        }
    }
}

/// Countdown driver that polls the [`Console`] for keys instead of
/// stdin, so cancel-on-keypress works on both the splash framebuffer
/// (input via `/dev/tty1`) and the raw-mode tty.
///
/// Thin adapter over the shared [`CountdownScreen`] widget: the widget
/// owns the deadline, the (rounded-up) tick cadence, and the render
/// callback, while THIS function supplies the selector's input policy via
/// the classifier. Today's policy is "cancel on the central layer's
/// one-shot `UserHasInteracted` — the single source of operator presence
/// — and ignore everything else", reproduced byte-for-byte below. The
/// wrapper already set the shared latch when it emitted that notice, so a
/// later screen this session sees the boot as attended without the
/// selector touching the latch itself. A `Resize` only repaints (the
/// selector loop redraws at the new geometry) and a raw `Key` arrives
/// *after* the notice, so neither cancels here.
async fn run_console_countdown(
    console: &mut dyn Console,
    app: &mut App<'_>,
    duration: Duration,
) -> Result<CountdownOutcome> {
    // The render callback paints the selector header with the remaining
    // seconds — exactly the old per-tick body. The widget hands back its
    // own `console`, so the closure renders through THAT borrow and only
    // captures `app`; the classifier captures nothing, keeping the two
    // closures' borrows disjoint.
    let render = |console: &mut dyn Console, secs: u64| -> Result<()> {
        app.countdown_remaining_secs = Some(secs);
        console.render(app)
    };
    // The selector's classifier: cancel ONLY on the presence notice.
    let classify = |event| {
        if matches!(event, crate::ui::console::ConsoleEvent::UserHasInteracted) {
            CountdownAction::Cancel
        } else {
            CountdownAction::Continue
        }
    };
    CountdownScreen::new(duration)
        .run(console, classify, render)
        .await
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

    use std::time::{Duration, Instant};

    use super::{
        Decision, FRAME_INTERVAL, render_gate, run_console_countdown, run_selector_on_console,
    };
    use crate::config::Config;
    use crate::error::Result;
    use crate::generations::Generation;
    use crate::ui::app::{App, SessionInteraction};
    use crate::ui::console::{Console, ConsoleEvent, ConsoleKind, LatchingConsole};
    use crate::ui::countdown::CountdownOutcome;

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
            toplevel: std::path::PathBuf::from(format!("/p/toplevel-{number}")),
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

    /// Console double that counts `render` calls and replays a canned
    /// burst of events. Every event is "already ready": `poll_event`
    /// returns one per call regardless of the timeout (so a held-key
    /// burst is fully queued), letting the loop's coalescing fold the
    /// whole burst into a single redraw. Once drained it yields `None`.
    struct RenderCountingConsole {
        keys: VecDeque<KeyEvent>,
        renders: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl RenderCountingConsole {
        fn new(keys: Vec<KeyEvent>) -> (Self, std::rc::Rc<std::cell::Cell<usize>>) {
            let renders = std::rc::Rc::new(std::cell::Cell::new(0));
            (
                Self {
                    keys: keys.into(),
                    renders: std::rc::Rc::clone(&renders),
                },
                renders,
            )
        }
    }

    impl Console for RenderCountingConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            self.renders.set(self.renders.get() + 1);
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

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn event_loop_coalesces_scroll_burst_into_few_redraws() {
        // A held arrow key in the log viewer is the lag's worst case: the
        // old loop drew + flushed one full frame per keystroke. The
        // coalescing loop drains the whole ready burst before a single
        // redraw, so M scroll events must collapse to far fewer than M
        // renders. We drive: open the log viewer (Ctrl+L), a burst of
        // Down presses, close it (Esc), then Enter to boot — all events
        // pre-queued so they are "ready" together.
        let cfg: Config = toml::from_str("").expect("default cfg");
        let gens = vec![sel_gen(1)];
        // Skip the countdown phase so we exercise the event loop directly.
        let session = SessionInteraction::new();
        session.set();

        const BURST: usize = 200;
        let mut script = Vec::with_capacity(BURST + 3);
        script.push(ctrl(KeyCode::Char('l')));
        for _ in 0..BURST {
            script.push(press(KeyCode::Down));
        }
        script.push(press(KeyCode::Esc));
        script.push(press(KeyCode::Enter));

        let (mut console, renders) = RenderCountingConsole::new(script);
        let decision = block(run_selector_on_console(
            &cfg,
            &gens,
            &mut console,
            0,
            &session,
        ))
        .expect("decision");
        assert!(matches!(decision, Decision::Boot { .. }));

        // The whole pre-queued burst drains in one inner-loop pass, so the
        // only redraw is the initial frame painted before the first poll.
        // Far below the old one-redraw-per-keystroke cost (which would be
        // > BURST renders).
        let count = renders.get();
        assert!(
            count <= 2,
            "coalescing must collapse a {BURST}-event burst to ≤2 redraws, got {count}"
        );
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
        assert_eq!(outcome, CountdownOutcome::Cancelled);
        assert!(
            session.get(),
            "cancelling the countdown must latch operator presence"
        );
    }

    // The frame-rate gate is a pure function of (dirty, last_render, now),
    // so these tests drive a controlled monotonic timeline (a base
    // `Instant` plus deltas) and assert the gate decision directly — no
    // real sleeps, fully deterministic.

    #[test]
    fn gate_renders_once_when_dirty_and_interval_elapsed() {
        // dirty + interval elapsed ⇒ render this pass; full POLL_SLICE wait
        // (the caller resets last_render and clears dirty on render).
        let last = Instant::now();
        let now = last + FRAME_INTERVAL;
        let gate = render_gate(true, last, now);
        assert!(gate.render, "interval has elapsed, must render");
        assert_eq!(gate.wait, super::POLL_SLICE);
    }

    #[test]
    fn gate_holds_render_and_bounds_wait_before_interval() {
        // dirty + interval NOT elapsed ⇒ no render this pass, and the poll
        // wait is bounded to the remaining time until the next allowed
        // render (≤ the remaining interval, and ≤ POLL_SLICE).
        let last = Instant::now();
        let elapsed = FRAME_INTERVAL / 3;
        let now = last + elapsed;
        let remaining = FRAME_INTERVAL - elapsed;
        let gate = render_gate(true, last, now);
        assert!(!gate.render, "interval not elapsed yet, must not render");
        assert!(
            gate.wait <= remaining,
            "wait {:?} must be bounded to remaining {:?}",
            gate.wait,
            remaining
        );
        assert!(gate.wait <= super::POLL_SLICE);
    }

    #[test]
    fn gate_paints_pending_frame_once_interval_passes_without_input() {
        // dirty + interval not elapsed, then time advances past the
        // interval with NO new events ⇒ the pending frame renders. This is
        // the deadline guarantee: the final state after a burst is never
        // lost.
        let last = Instant::now();
        // First pass: too early, held.
        let early = render_gate(true, last, last + FRAME_INTERVAL / 2);
        assert!(!early.render);
        // Time advances past the interval (the bounded wait woke us); still
        // dirty, no new input ⇒ now it paints.
        let late = render_gate(true, last, last + FRAME_INTERVAL + Duration::from_millis(1));
        assert!(late.render, "pending frame must paint once interval passes");
    }

    #[test]
    fn gate_never_renders_when_not_dirty() {
        // not dirty ⇒ never render regardless of how much time elapsed,
        // and the wait stays the full POLL_SLICE (no deadline to chase).
        let last = Instant::now();
        let way_past = last + FRAME_INTERVAL * 100;
        let gate = render_gate(false, last, way_past);
        assert!(!gate.render, "nothing changed, must never render");
        assert_eq!(gate.wait, super::POLL_SLICE);
    }
}
