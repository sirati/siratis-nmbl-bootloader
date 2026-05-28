//! Emergency-screen orchestrator.
//!
//! When a top-level phase returns `Err`, `shell::drop_to_emergency`
//! used to immediately `execve` the shell. That bypassed the splash
//! backend entirely: operators on a VNC console saw nothing useful,
//! and there was no way to choose between rebooting and dropping to
//! the shell. This module replaces that behaviour with a proper TUI
//! that renders into the already-open boot [`Console`].
//!
//! Architectural rule: **all UI is TUI code**. The splash backend is
//! only a render target. The state machine that drives the screen
//! lives in [`crate::ui::app::Screen::Emergency`]; the renderer lives
//! in [`crate::ui::view::render_emergency`]. This module wires the
//! two together against the caller-supplied console and applies a
//! 30-second default-to-reboot timer.
//!
//! ## Console ownership
//!
//! The boot orchestrator (main.rs) brings the [`Console`] up once at
//! boot, hands it through every phase, and — on phase failure — passes
//! the same handle into [`run_emergency_screen`]. The backend choice
//! (splash vs tty, with panic-recovery skipping splash) is therefore
//! already made by [`crate::ui::console::open_console`]; this module
//! is purely a state-machine driver. The serial-console code path is
//! the operator's existing tty console — `/dev/console` already routes
//! to the serial line in that deployment.
//!
//! ## Timer
//!
//! With no input for 30 seconds we default to [`EmergencyChoice::Reboot`].
//! Operators on a remote VNC console may not be sitting there when boot
//! fails; rebooting is the safe default — if the next boot also fails
//! they'll just land back here.
//!
//! The clock is injected as a `Fn() -> Instant` so unit tests can run
//! the timer machinery without sleeping a real wall-clock second.

use std::time::{Duration, Instant};

use crate::error::{NmblError, Result, format_chain};
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, EmergencyChoice, EmergencyItem, Screen};
use crate::ui::console::Console;

/// Default countdown to auto-reboot when the operator is not present.
const EMERGENCY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the emergency screen on the supplied [`Console`] and return the
/// operator's choice.
///
/// The caller (main.rs / shell.rs) owns the console lifecycle: this
/// function only drives the TUI event loop. With no input for 30s the
/// timer expires and the function returns [`EmergencyChoice::Reboot`].
/// On a backend error mid-loop we also fall back to Reboot — the
/// safest default when the operator can't see the screen anyway.
///
/// One-shot convenience used by tests and any caller that hasn't yet
/// adopted the persistent-App overlay model. The auto-reboot countdown
/// starts fresh on every call. Production code uses
/// [`run_emergency_screen_with_app`] instead so re-entries don't
/// restart the timer.
pub fn run_emergency_screen(console: &mut dyn Console, err: &NmblError) -> EmergencyChoice {
    let message = build_message(err);
    let items = default_items();
    let mut app = build_emergency_app(&message, &items);
    run_emergency_screen_with_app(console, &mut app)
}

/// Same as [`run_emergency_screen`] but reuses an externally-owned
/// `App` so the auto-reboot countdown deadline (held in
/// `app.error_countdown_deadline`) survives a re-entry after the
/// operator dismisses a modal and lands back on the error screen.
///
/// On first call the helper latches the deadline at `now + 30s`; on
/// re-entry the existing deadline is preserved. If the deadline has
/// already elapsed on re-entry the loop reboots immediately.
pub fn run_emergency_screen_with_app(
    console: &mut dyn Console,
    app: &mut App<'_>,
) -> EmergencyChoice {
    // The loop itself latches on first entry — subsequent calls find
    // Some(_) and keep the original deadline. Re-entry after an
    // elapsed deadline trips the "remaining = None" branch inside the
    // loop and returns Reboot at once.
    drive_emergency_loop(app, EMERGENCY_TIMEOUT, Instant::now, console)
        .unwrap_or(EmergencyChoice::Reboot)
}

/// Build the message string shown to the operator. Includes the
/// suggested-action hint plus the formatted error chain.
pub(crate) fn build_message(err: &NmblError) -> String {
    let mut s = String::new();
    s.push_str("Boot failed. The chain of errors is:\n\n");
    s.push_str(&format_chain(err as &dyn std::error::Error));
    s.push_str("\n\nChoose what to do next.");
    s
}

/// Items shown on the emergency screen. Order matters: index 0 is the
/// default if the operator just presses Enter, and it's what the
/// timeout rolls over to.
///
/// `Pretty Shell` is inserted between `Reboot` and `Raw Shell` only
/// when the `image-splash` Cargo feature is compiled in — it depends
/// on the `alacritty_terminal` parser which is only an optional dep of
/// that feature. When the feature is on Pretty Shell is the preferred
/// recovery shell; the raw busybox-on-tty path sits below it as a
/// fallback. The `Retry boot from config` and `Verify kexec readiness`
/// actions are unconditional: they only need the existing phase 3/4/5
/// plumbing already in the binary.
pub(crate) fn default_items() -> Vec<EmergencyItem> {
    // `mut` is conditionally used (the `insert` below is feature-gated);
    // suppress the unused_mut warning on no-feature builds without
    // duplicating the vec literal.
    #[cfg_attr(not(feature = "image-splash"), allow(unused_mut))]
    let mut items = vec![EmergencyItem {
        label: "Reboot",
        choice: EmergencyChoice::Reboot,
    }];
    #[cfg(feature = "image-splash")]
    items.push(EmergencyItem {
        label: "Pretty Shell",
        choice: EmergencyChoice::PrettyShell,
    });
    items.push(EmergencyItem {
        label: "Raw Shell",
        choice: EmergencyChoice::RawShell,
    });
    items.push(EmergencyItem {
        label: "Retry boot from config",
        choice: EmergencyChoice::RetryBoot,
    });
    items.push(EmergencyItem {
        label: "Verify kexec readiness",
        choice: EmergencyChoice::VerifyKexecReadiness,
    });
    items
}

/// Build an `App` parked on the Emergency screen with the given
/// message and items.
pub(crate) fn build_emergency_app<'a>(
    message: &str,
    items_template: &[EmergencyItem],
) -> App<'a> {
    // Items are tiny, no point fighting the borrow checker — clone
    // the template into the App's own Screen state.
    let items: Vec<EmergencyItem> = items_template
        .iter()
        .map(|it| EmergencyItem {
            label: it.label,
            choice: it.choice,
        })
        .collect();
    let mut app = App::new(&[]);
    app.screen = Screen::Emergency {
        message: message.to_owned(),
        items,
        selected: 0,
        chosen: None,
    };
    app
}

/// Shared event-loop driver. Render, poll, react, repeat — and apply
/// the no-input timeout. Returns the operator's choice or the default
/// when the timer expires.
///
/// `now` is injected so tests can drive the timeout machinery without
/// real wall-clock waits.
fn drive_emergency_loop<N>(
    app: &mut App<'_>,
    timeout: Duration,
    now: N,
    console: &mut dyn Console,
) -> Result<EmergencyChoice>
where
    N: Fn() -> Instant,
{
    // Latch on first entry — `latch_error_countdown` is a no-op when
    // the deadline is already `Some(_)`, so re-entries after a modal
    // dismissal preserve the original wall-clock deadline. A session
    // in which the operator has already pressed a key has its
    // deadline cleared (see the keypress branch below); on the next
    // loop tick we observe `error_countdown_deadline == None` and
    // the latch fires again — that is correct on first entry but
    // wrong inside a still-running loop. The latch therefore lives
    // here at function entry only.
    app.latch_error_countdown(timeout);

    // Mirror the deadline into the App's display field. Only set the
    // displayed remaining-seconds if the deadline is armed; a session
    // that the operator already touched has `error_countdown_deadline
    // == None` and shows no countdown.
    let initial_secs = match app.error_countdown_deadline {
        Some(d) => match d.checked_duration_since(now()) {
            Some(r) => {
                let s = r.as_secs();
                app.countdown_remaining_secs = Some(s);
                s
            }
            None => {
                // Deadline already in the past on entry — reboot
                // immediately. Matches the spec's "past_instant" case.
                return Ok(EmergencyChoice::Reboot);
            }
        },
        None => {
            // No deadline armed — the countdown UI stays hidden and
            // the loop only exits on keypress.
            app.countdown_remaining_secs = None;
            0
        }
    };
    let mut last_reported = initial_secs;

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
                None => {
                    // Timer expired. Default to Reboot.
                    return Ok(EmergencyChoice::Reboot);
                }
            },
            None => POLL_SLICE,
        };

        if let Some(key) = console.poll_key(slice)? {
            // Any keypress cancels the auto-reboot countdown for the
            // remainder of this session: clear both the display field
            // and the latched deadline so re-entries don't re-arm it
            // and so the loop body above falls through to "no
            // deadline" handling.
            app.countdown_remaining_secs = None;
            app.error_countdown_deadline = None;
            if app.on_key(key) {
                break;
            }
            dirty = true;
            continue;
        }

        // No input this slice. Tick the displayed countdown if the
        // visible second has changed. Skipped when the deadline is
        // disarmed (operator already touched the menu).
        if let Some(d) = app.error_countdown_deadline {
            let Some(remaining) = d.checked_duration_since(now()) else {
                return Ok(EmergencyChoice::Reboot);
            };
            let secs = remaining.as_secs();
            if secs != last_reported {
                app.countdown_remaining_secs = Some(secs);
                last_reported = secs;
                dirty = true;
            }
        }
    }

    match &app.screen {
        Screen::Emergency { chosen, .. } => Ok(chosen.unwrap_or(EmergencyChoice::Reboot)),
        _ => Err(NmblError::Tui {
            source: std::io::Error::other("emergency screen exited off-screen"),
        }),
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
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::Cell;

    use crate::ui::console::{ConsoleEvent, ConsoleKind};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// In-process [`Console`] for unit-testing the emergency loop.
    /// Drives a scripted sequence of key events on `poll_event()` and
    /// counts renders.
    struct TestConsole {
        events: Vec<Option<KeyEvent>>,
        cursor: usize,
        renders: u32,
    }

    impl TestConsole {
        fn new(events: Vec<Option<KeyEvent>>) -> Self {
            Self {
                events,
                cursor: 0,
                renders: 0,
            }
        }
    }

    impl Console for TestConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn poll_event(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            let v = self.events.get(self.cursor).copied().flatten();
            self.cursor = self.cursor.saturating_add(1);
            Ok(v.map(ConsoleEvent::Key))
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(
            &mut self,
            _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
        ) -> Result<()> {
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn build_message_includes_error_chain_lines() {
        let err = NmblError::Io {
            source: std::io::Error::other("disk on fire"),
            context: "mounting /tmp".to_string(),
        };
        let msg = build_message(&err);
        assert!(msg.contains("mounting /tmp"), "expected context: {msg}");
        assert!(msg.contains("disk on fire"), "expected source: {msg}");
    }

    #[test]
    fn drive_emergency_loop_returns_reboot_on_timeout() {
        // Advance the injected clock past the deadline on the very
        // first poll. No keys are ever delivered, so the loop must
        // bail out with the default (Reboot) rather than spinning.
        let now_calls = Cell::new(0u32);
        let fake_now = || {
            let n = now_calls.get();
            now_calls.set(n.saturating_add(1));
            // Start at a fixed epoch, then jump way past it.
            let base = Instant::now();
            if n < 1 {
                base
            } else {
                base + Duration::from_secs(60)
            }
        };

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut console = TestConsole::new(vec![None; 16]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, fake_now, &mut console)
            .expect("loop must not error on timeout path");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        assert!(console.renders >= 1, "must render at least one frame");
    }

    #[test]
    fn drive_emergency_loop_returns_selected_on_enter() {
        // Press the 's' hotkey to commit the Raw Shell entry. The
        // hotkey path is feature-independent, so this test stays
        // stable whether or not `image-splash` adds a Pretty Shell row
        // between the Reboot and Raw Shell rows in default_items. The
        // clock never advances, so the timeout never fires.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut console = TestConsole::new(vec![Some(press(KeyCode::Char('s')))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must not error on the happy path");
        assert_eq!(outcome, EmergencyChoice::RawShell);
    }

    #[test]
    fn drive_emergency_loop_keypress_cancels_countdown() {
        // Press 'r' immediately, before the timer matters. Choice must
        // be Reboot and BOTH countdown fields must have been cleared.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut console = TestConsole::new(vec![Some(press(KeyCode::Char('r')))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must succeed");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        assert!(app.countdown_remaining_secs.is_none());
        assert!(
            app.error_countdown_deadline.is_none(),
            "keypress must disarm the deadline latch"
        );
    }

    #[test]
    fn drive_emergency_loop_fresh_entry_latches_deadline_and_sets_countdown() {
        // Fresh App: no deadline armed. After one render the latch
        // must have fired and the displayed countdown must be set.
        // We capture `t_before` BEFORE calling the loop so the
        // injected clock sits strictly earlier than the (real)
        // `Instant::now()` used inside `latch_error_countdown`; that
        // guarantees `remaining >= 30s` and `as_secs() == 30`.
        let t_before = Instant::now();
        let frozen_now = move || t_before;

        let mut app = build_emergency_app("boot failed", &default_items());
        assert!(app.error_countdown_deadline.is_none());
        // One render then commit Reboot so the loop exits cleanly.
        let mut console = TestConsole::new(vec![None, Some(press(KeyCode::Char('r')))]);

        let _ = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must succeed");

        assert!(
            app.error_countdown_deadline.is_none(),
            "the trailing keypress must have cleared the deadline again"
        );
        // The crucial check is that the loop COULD set both fields
        // when starting from None. Verify by running a non-committing
        // sequence and inspecting the App after the first render but
        // before any keypress: drive a fresh entry with no events,
        // letting the timeout fire.
        let mut app2 = build_emergency_app("boot failed", &default_items());
        let t_before2 = Instant::now();
        // First call returns t_before2 (deadline still future), second
        // call jumps far past the deadline so the loop exits Reboot.
        let calls = Cell::new(0u32);
        let staggered_now = || {
            let n = calls.get();
            calls.set(n.saturating_add(1));
            if n < 2 { t_before2 } else { t_before2 + Duration::from_secs(120) }
        };
        let mut console2 = TestConsole::new(vec![None]);
        let outcome =
            drive_emergency_loop(&mut app2, EMERGENCY_TIMEOUT, staggered_now, &mut console2)
                .expect("loop must succeed");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        // Deadline was latched before the loop body ticked. It stays
        // Some(_) on the timeout path (no keypress cleared it).
        assert!(
            app2.error_countdown_deadline.is_some(),
            "fresh entry must latch the deadline"
        );
        assert_eq!(
            app2.countdown_remaining_secs,
            Some(EMERGENCY_TIMEOUT.as_secs()),
            "fresh entry must display the full timeout"
        );
    }

    #[test]
    fn drive_emergency_loop_preserves_existing_future_deadline() {
        // Pre-arm a deadline 45s in the future. Latch must be a no-op
        // and the original deadline must survive the loop. We commit
        // Reboot via 'r' on the first event so the keypress branch
        // clears the deadline — to test PRESERVATION we therefore
        // sample the deadline before the keypress by running a
        // non-committing tick first (None → tick → 'r').
        let preset_now = Instant::now();
        let preset_deadline = preset_now + Duration::from_secs(45);
        let frozen_now = move || preset_now;

        let mut app = build_emergency_app("boot failed", &default_items());
        app.error_countdown_deadline = Some(preset_deadline);

        // Capture state after the first non-committing tick using a
        // probe console that reads the App between calls.
        struct ProbeConsole {
            renders: u32,
            captured_deadline: Cell<Option<Instant>>,
            captured_secs: Cell<Option<u64>>,
        }
        impl Console for ProbeConsole {
            fn render(&mut self, app: &App<'_>) -> Result<()> {
                if self.renders == 0 {
                    self.captured_deadline.set(app.error_countdown_deadline);
                    self.captured_secs.set(app.countdown_remaining_secs);
                }
                self.renders = self.renders.saturating_add(1);
                Ok(())
            }
            fn poll_event(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
                // Capture happened during the single render before
                // this first poll; commit Reboot now so the loop
                // exits cleanly (with the frozen clock the countdown
                // never ticks, so no second render would otherwise
                // ever fire and the loop would spin forever).
                Ok(Some(ConsoleEvent::Key(press(KeyCode::Char('r')))))
            }
            fn size(&self) -> (u16, u16) {
                (80, 24)
            }
            fn kind(&self) -> ConsoleKind {
                ConsoleKind::Tty
            }
            fn draw_with(
                &mut self,
                _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
            ) -> Result<()> {
                Ok(())
            }
            fn suspend(&mut self) -> Result<()> {
                Ok(())
            }
            fn resume(&mut self) -> Result<()> {
                Ok(())
            }
        }
        let mut console = ProbeConsole {
            renders: 0,
            captured_deadline: Cell::new(None),
            captured_secs: Cell::new(None),
        };

        let _ = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must succeed");

        assert_eq!(
            console.captured_deadline.get(),
            Some(preset_deadline),
            "existing future deadline must be preserved verbatim"
        );
        assert_eq!(
            console.captured_secs.get(),
            Some(45),
            "countdown display must reflect the preserved deadline"
        );
    }

    #[test]
    fn drive_emergency_loop_past_deadline_reboots_immediately() {
        // Pre-arm a deadline in the past. The loop must return Reboot
        // before consuming any input.
        let preset_now = Instant::now();
        let past = preset_now - Duration::from_secs(10);
        let frozen_now = move || preset_now;

        let mut app = build_emergency_app("boot failed", &default_items());
        app.error_countdown_deadline = Some(past);

        // No events scripted: if the loop tried to poll, it would
        // get None forever and spin (the test would hang). We rely
        // on the early-return guard.
        let mut console = TestConsole::new(vec![]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must succeed");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        // Deadline preserved (the keypress branch never fired).
        assert_eq!(app.error_countdown_deadline, Some(past));
    }

    #[test]
    fn drive_emergency_loop_keypress_disarms_deadline_for_session() {
        // Non-committing keypress (Down) followed by a timeout's
        // worth of empty polls. Bug A regression: the original code
        // hid the display but the local Instant deadline still
        // fired. After the fix, ANY keypress disarms the deadline
        // and subsequent empty polls must NOT reboot.
        let start = Instant::now();
        // The injected clock progresses way past the original 30s
        // deadline after the first call. If the deadline were still
        // armed, the second iteration would return Reboot. After
        // the fix, the deadline is None and we keep looping.
        let calls = Cell::new(0u32);
        let staggered_now = || {
            let n = calls.get();
            calls.set(n.saturating_add(1));
            if n < 2 { start } else { start + Duration::from_secs(120) }
        };

        let mut app = build_emergency_app("boot failed", &default_items());
        // Sequence: Down (non-committing, disarms), then a few Nones
        // (loop must NOT reboot), then 'r' to exit cleanly.
        let mut console = TestConsole::new(vec![
            Some(press(KeyCode::Down)),
            None,
            None,
            None,
            Some(press(KeyCode::Char('r'))),
        ]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, staggered_now, &mut console)
            .expect("loop must succeed");
        // Outcome is Reboot only because we explicitly pressed 'r',
        // NOT because the timer fired. The deadline was disarmed by
        // the earlier Down keypress and never re-armed.
        assert_eq!(outcome, EmergencyChoice::Reboot);
        assert!(
            app.error_countdown_deadline.is_none(),
            "deadline must remain None after a non-committing keypress"
        );
        assert!(app.countdown_remaining_secs.is_none());
    }

    #[test]
    fn run_emergency_screen_returns_reboot_when_render_errors() {
        // Console that always errors on render. The public entry point
        // must swallow the error and fall back to Reboot — the safest
        // default when the operator can't see the screen.
        struct BrokenConsole;
        impl Console for BrokenConsole {
            fn render(&mut self, _app: &App<'_>) -> Result<()> {
                Err(NmblError::Tui {
                    source: std::io::Error::other("backend dead"),
                })
            }
            fn poll_event(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
                Ok(None)
            }
            fn size(&self) -> (u16, u16) {
                (0, 0)
            }
            fn kind(&self) -> ConsoleKind {
                ConsoleKind::Tty
            }
            fn draw_with(
                &mut self,
                _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
            ) -> Result<()> {
                Err(NmblError::Tui {
                    source: std::io::Error::other("backend dead"),
                })
            }
            fn suspend(&mut self) -> Result<()> {
                Ok(())
            }
            fn resume(&mut self) -> Result<()> {
                Ok(())
            }
        }

        let err = NmblError::Io {
            source: std::io::Error::other("kaboom"),
            context: "test".to_string(),
        };
        let mut console = BrokenConsole;
        let choice = run_emergency_screen(&mut console, &err);
        assert_eq!(choice, EmergencyChoice::Reboot);
    }

    #[test]
    fn default_items_first_is_reboot() {
        // The whole "timeout defaults to Reboot" contract hangs on
        // Reboot being the first item; if a future refactor flips the
        // order, the timeout test still passes but production gets
        // surprising behaviour. Pin the contract here.
        let items = default_items();
        assert_eq!(items[0].choice, EmergencyChoice::Reboot);
        // With `image-splash` the Pretty Shell entry sits at index 1
        // and the Raw Shell entry at index 2; without the feature the
        // Raw Shell entry falls back to index 1.
        #[cfg(feature = "image-splash")]
        {
            assert_eq!(items[1].choice, EmergencyChoice::PrettyShell);
            assert_eq!(items[2].choice, EmergencyChoice::RawShell);
        }
        #[cfg(not(feature = "image-splash"))]
        {
            assert_eq!(items[1].choice, EmergencyChoice::RawShell);
        }
    }

    #[test]
    fn default_items_includes_retry_and_verify_in_order() {
        // The dispatcher in `shell.rs` matches on these variants by
        // name; the order pinned here is what the operator actually
        // sees on the picker. Reboot comes first (muscle-memory + the
        // 30s timeout default), then Pretty Shell (feature-gated, the
        // preferred recovery shell when available), then Raw Shell,
        // then RetryBoot, then VerifyKexecReadiness — most-destructive
        // to least-destructive, so a stray Enter on the default
        // doesn't kick off an in-process retry the operator didn't
        // want.
        let items = default_items();
        let choices: Vec<EmergencyChoice> = items.iter().map(|it| it.choice).collect();

        let mut expected: Vec<EmergencyChoice> = vec![EmergencyChoice::Reboot];
        #[cfg(feature = "image-splash")]
        expected.push(EmergencyChoice::PrettyShell);
        expected.push(EmergencyChoice::RawShell);
        expected.push(EmergencyChoice::RetryBoot);
        expected.push(EmergencyChoice::VerifyKexecReadiness);

        assert_eq!(choices, expected, "default_items order has drifted");
    }

    #[test]
    fn default_items_labels_match_spec() {
        // The labels appear verbatim in the emergency picker; pin
        // them so a relabel doesn't slip past review (the empirical
        // verification step greps for these strings).
        let items = default_items();
        let labels: Vec<&str> = items.iter().map(|it| it.label).collect();
        assert!(
            labels.contains(&"Retry boot from config"),
            "missing 'Retry boot from config' in {labels:?}"
        );
        assert!(
            labels.contains(&"Verify kexec readiness"),
            "missing 'Verify kexec readiness' in {labels:?}"
        );
    }
}
