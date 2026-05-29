//! Behavioural tests for `drive_emergency_loop` and the public
//! `run_emergency_screen*` entry points.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crossterm::event::KeyCode;

use crate::error::{NmblError, Result};
use crate::ui::app::{App, EmergencyChoice, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

use super::super::{
    EMERGENCY_TIMEOUT, build_emergency_app, default_items, loop_driver::drive_emergency_loop,
    run_emergency_screen,
};
use super::{TestConsole, block, fresh_emergency_app, press};

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

    let mut app = fresh_emergency_app("boot failed");
    let mut console = TestConsole::new(vec![None; 16]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        fake_now,
        &mut console,
    ))
    .expect("loop must not error on timeout path");
    assert_eq!(outcome, EmergencyChoice::Reboot);
    assert!(console.renders >= 1, "must render at least one frame");
}

#[test]
fn drive_emergency_loop_returns_selected_on_enter() {
    // Press the 's' hotkey to commit the Raw Shell entry. The
    // hotkey path is feature-independent, so this test stays
    // stable whether or not `pretty-shell` adds a Pretty Shell row
    // between the Reboot and Raw Shell rows in default_items. The
    // clock never advances, so the timeout never fires.
    let start = Instant::now();
    let frozen_now = move || start;

    let mut app = fresh_emergency_app("boot failed");
    let mut console = TestConsole::new(vec![Some(press(KeyCode::Char('s')))]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        frozen_now,
        &mut console,
    ))
    .expect("loop must not error on the happy path");
    assert_eq!(outcome, EmergencyChoice::RawShell);
}

#[test]
fn drive_emergency_loop_keypress_cancels_countdown() {
    // Press 'r' immediately, before the timer matters. Choice must
    // be Reboot and BOTH countdown fields must have been cleared.
    let start = Instant::now();
    let frozen_now = move || start;

    let mut app = fresh_emergency_app("boot failed");
    let mut console = TestConsole::new(vec![Some(press(KeyCode::Char('r')))]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        frozen_now,
        &mut console,
    ))
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

    let mut app = fresh_emergency_app("boot failed");
    assert!(app.error_countdown_deadline.is_none());
    // One render then commit Reboot so the loop exits cleanly.
    let mut console = TestConsole::new(vec![None, Some(press(KeyCode::Char('r')))]);

    let _ = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        frozen_now,
        &mut console,
    ))
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
    let mut app2 = fresh_emergency_app("boot failed");
    let t_before2 = Instant::now();
    // First call returns t_before2 (deadline still future), second
    // call jumps far past the deadline so the loop exits Reboot.
    let calls = Cell::new(0u32);
    let staggered_now = || {
        let n = calls.get();
        calls.set(n.saturating_add(1));
        if n < 2 {
            t_before2
        } else {
            t_before2 + Duration::from_secs(120)
        }
    };
    let mut console2 = TestConsole::new(vec![None]);
    let outcome = block(drive_emergency_loop(
        &mut app2,
        EMERGENCY_TIMEOUT,
        staggered_now,
        &mut console2,
    ))
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

    let mut app = fresh_emergency_app("boot failed");
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
        fn poll_event<'a>(
            &'a mut self,
            timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
        {
            Box::pin(async move { self.poll_event_blocking(timeout) })
        }
        fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
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
    let mut console = ProbeConsole {
        renders: 0,
        captured_deadline: Cell::new(None),
        captured_secs: Cell::new(None),
    };

    let _ = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        frozen_now,
        &mut console,
    ))
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

    let mut app = fresh_emergency_app("boot failed");
    app.error_countdown_deadline = Some(past);

    // No events scripted: if the loop tried to poll, it would
    // get None forever and spin (the test would hang). We rely
    // on the early-return guard.
    let mut console = TestConsole::new(vec![]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        frozen_now,
        &mut console,
    ))
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
        if n < 2 {
            start
        } else {
            start + Duration::from_secs(120)
        }
    };

    let mut app = fresh_emergency_app("boot failed");
    // Sequence: Down (non-committing, disarms), then a few Nones
    // (loop must NOT reboot), then 'r' to exit cleanly.
    let mut console = TestConsole::new(vec![
        Some(press(KeyCode::Down)),
        None,
        None,
        None,
        Some(press(KeyCode::Char('r'))),
    ]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        staggered_now,
        &mut console,
    ))
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
fn drive_emergency_loop_no_countdown_when_user_interacted() {
    // Attended boot: the operator already pressed a key earlier
    // (e.g. a LUKS passphrase). Even with the clock jumping far
    // past the would-be deadline, the loop must NOT arm a timer or
    // reboot on timeout — it waits for an explicit choice.
    let start = Instant::now();
    let calls = Cell::new(0u32);
    let staggered_now = || {
        let n = calls.get();
        calls.set(n.saturating_add(1));
        if n < 1 {
            start
        } else {
            start + Duration::from_secs(120)
        }
    };

    // Set the shared session latch to mark the boot as attended.
    let session = SessionInteraction::new();
    session.set();
    let mut app = build_emergency_app("boot failed", &default_items(), &session);
    // A few empty polls (which would have tripped a timeout reboot
    // if a deadline were armed) then an explicit 'r' to exit.
    let mut console = TestConsole::new(vec![None, None, None, Some(press(KeyCode::Char('r')))]);

    let outcome = block(drive_emergency_loop(
        &mut app,
        EMERGENCY_TIMEOUT,
        staggered_now,
        &mut console,
    ))
    .expect("loop must succeed");
    assert_eq!(outcome, EmergencyChoice::Reboot);
    assert!(
        app.error_countdown_deadline.is_none(),
        "no deadline may be armed on an attended boot"
    );
    assert!(
        app.countdown_remaining_secs.is_none(),
        "no countdown may be displayed on an attended boot"
    );
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
        fn poll_event<'a>(
            &'a mut self,
            timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
        {
            Box::pin(async move { self.poll_event_blocking(timeout) })
        }
        fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            (0, 0)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
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
    let choice = block(run_emergency_screen(&mut console, &err));
    assert_eq!(choice, EmergencyChoice::Reboot);
}
