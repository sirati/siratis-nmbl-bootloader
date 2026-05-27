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
pub fn run_emergency_screen(console: &mut dyn Console, err: &NmblError) -> EmergencyChoice {
    let message = build_message(err);
    let items = default_items();
    let mut app = build_emergency_app(&message, &items);
    drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, Instant::now, console)
        .unwrap_or(EmergencyChoice::Reboot)
}

/// Build the message string shown to the operator. Includes the
/// suggested-action hint plus the formatted error chain.
fn build_message(err: &NmblError) -> String {
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
/// `Pretty Shell` is inserted between `Shell` and the retry/verify
/// items only when the `image-splash` Cargo feature is compiled in —
/// it depends on the `alacritty_terminal` parser which is only an
/// optional dep of that feature. The `Retry boot from config` and
/// `Verify kexec readiness` actions are unconditional: they only need
/// the existing phase 3/4/5 plumbing already in the binary.
fn default_items() -> Vec<EmergencyItem> {
    // `mut` is conditionally used (the `insert` below is feature-gated);
    // suppress the unused_mut warning on no-feature builds without
    // duplicating the vec literal.
    #[cfg_attr(not(feature = "image-splash"), allow(unused_mut))]
    let mut items = vec![
        EmergencyItem {
            label: "Reboot",
            choice: EmergencyChoice::Reboot,
        },
        EmergencyItem {
            label: "Shell",
            choice: EmergencyChoice::Shell,
        },
    ];
    #[cfg(feature = "image-splash")]
    items.push(EmergencyItem {
        label: "Pretty Shell",
        choice: EmergencyChoice::PrettyShell,
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
fn build_emergency_app<'a>(message: &str, items_template: &[EmergencyItem]) -> App<'a> {
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
    let start = now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    let initial_secs = timeout.as_secs();
    app.countdown_remaining_secs = Some(initial_secs);
    let mut last_reported = initial_secs;

    let mut dirty = true;
    loop {
        if dirty {
            console.render(app)?;
            dirty = false;
        }

        let current = now();
        let remaining = match deadline.checked_duration_since(current) {
            Some(r) => r,
            None => {
                // Timer expired. Default to Reboot (the first item).
                return Ok(EmergencyChoice::Reboot);
            }
        };
        let slice = remaining.min(POLL_SLICE);

        if let Some(key) = console.poll_key(slice)? {
            // Any keypress cancels the auto-reboot countdown so the
            // operator can take their time deciding once present.
            app.countdown_remaining_secs = None;
            if app.on_key(key) {
                break;
            }
            dirty = true;
            continue;
        }

        // No input this slice. Tick the countdown if the displayed
        // second has changed. We only show the countdown until the
        // first keypress.
        if app.countdown_remaining_secs.is_some() {
            let current = now();
            let Some(remaining) = deadline.checked_duration_since(current) else {
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

    use crate::ui::console::ConsoleKind;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// In-process [`Console`] for unit-testing the emergency loop.
    /// Drives a scripted sequence of key events on `poll_key()` and
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
        fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            let v = self.events.get(self.cursor).copied().flatten();
            self.cursor = self.cursor.saturating_add(1);
            Ok(v)
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
        // Move down once, press Enter. Expected choice: Shell.
        // The clock never advances, so the timeout never fires.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut console =
            TestConsole::new(vec![Some(press(KeyCode::Down)), Some(press(KeyCode::Enter))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must not error on the happy path");
        assert_eq!(outcome, EmergencyChoice::Shell);
    }

    #[test]
    fn drive_emergency_loop_keypress_cancels_countdown() {
        // Press 'r' immediately, before the timer matters. Choice must
        // be Reboot and the countdown must have been cleared.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut console = TestConsole::new(vec![Some(press(KeyCode::Char('r')))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut console)
            .expect("loop must succeed");
        assert_eq!(outcome, EmergencyChoice::Reboot);
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
            fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
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
        assert_eq!(items[1].choice, EmergencyChoice::Shell);
    }

    #[test]
    fn default_items_includes_retry_and_verify_in_order() {
        // The dispatcher in `shell.rs` matches on these variants by
        // name; the order pinned here is what the operator actually
        // sees on the picker. Reboot and Shell come first (muscle-
        // memory), then PrettyShell (feature-gated), then RetryBoot,
        // then VerifyKexecReadiness — most-destructive to least-
        // destructive, so a stray Enter on the default doesn't kick
        // off an in-process retry the operator didn't want.
        let items = default_items();
        let choices: Vec<EmergencyChoice> = items.iter().map(|it| it.choice).collect();

        let mut expected: Vec<EmergencyChoice> =
            vec![EmergencyChoice::Reboot, EmergencyChoice::Shell];
        #[cfg(feature = "image-splash")]
        expected.push(EmergencyChoice::PrettyShell);
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
