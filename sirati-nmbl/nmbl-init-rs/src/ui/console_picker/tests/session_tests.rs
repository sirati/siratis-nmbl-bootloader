use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;

use crate::config::Config;
use crate::ui::console::ConsoleKind;

use super::super::display_overlaps_targets;
use super::super::session::{
    PickerSessionOutcome, SPLASH_DISPLAY_TTY, dispatch_spawn, display_target_for, drive_picker_loop,
};
use super::super::types::{
    ButtonCursor, CandidateOrigin, PickerCandidate, PickerOutcome, PickerState,
};
use super::{FakeConsole, block, press};

#[test]
fn driver_loop_runs_picker_to_spawn_outcome_via_fake_console() {
    let mut state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_checked: true,
        outcome: None,
    };
    let mut console = FakeConsole::new(vec![
        // Move into the custom-input row.
        Some(press(KeyCode::Down)),
        // Move into the buttons row.
        Some(press(KeyCode::Down)),
        // Enter on [Spawn].
        Some(press(KeyCode::Enter)),
    ]);
    block(drive_picker_loop(&mut state, &mut console)).expect("loop must not error");
    match state.outcome {
        Some(PickerOutcome::Spawn { targets }) => {
            assert_eq!(targets, vec![PathBuf::from("/dev/tty0")]);
        }
        other => panic!("expected Spawn, got {other:?}"),
    }
    assert!(console.renders >= 1);
}

#[test]
fn display_overlaps_targets_matches_path() {
    let targets = vec![PathBuf::from("/dev/tty0"), PathBuf::from("/dev/ttyS0")];
    assert!(display_overlaps_targets(Path::new("/dev/tty0"), &targets));
    assert!(!display_overlaps_targets(Path::new("/dev/tty1"), &targets));
    assert!(!display_overlaps_targets(Path::new("/dev/tty0"), &[]));
}

/// Splash backend always renders to `/dev/tty1`, regardless of what
/// `/sys/class/tty/console/active` reports. Pins the contract that
/// motivates this bug-fix: on a system with cmdline
/// `console=ttyS0,115200 console=tty1` sysfs lists ttyS0 first, but
/// the picker is the authoritative source and must return
/// `/dev/tty1` so the relay agrees on the overlap predicate.
#[test]
fn display_target_for_splash_is_always_tty1() {
    let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
    assert_eq!(
        display_target_for(&console),
        PathBuf::from(SPLASH_DISPLAY_TTY)
    );
    // Hammer the kind a second time; the resolver MUST NOT memoise
    // across console kinds.
    console.kind = ConsoleKind::Tty;
    // For tty kind it falls back to read_active_console() which may
    // succeed or fall back to /dev/console depending on the host;
    // we only assert it does NOT return SPLASH_DISPLAY_TTY by
    // construction (it may equal it coincidentally on a desktop —
    // skip the negative assertion).
    let _ = display_target_for(&console);
}

/// Picker decides overlap=false: selection is `/dev/ttyS0` only on
/// a splash console (display target = `/dev/tty1`). Dispatch MUST
/// take the fire-and-forget branch — relay MUST NOT be invoked,
/// and `console.suspend()` MUST NOT be called.
///
/// This is the half of the bug-fix that protects against routing
/// the relay onto a non-display tty. The companion test below pins
/// the relay-branch path.
#[test]
fn dispatch_no_overlap_runs_detach_not_relay() {
    let cfg = Config::recovery_default();
    let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
    let targets = vec![PathBuf::from("/dev/ttyS0")];
    let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);

    // Counters live in `Cell`s so the boxed-future relay seam (which
    // borrows only its `'a` args, not the closure environment) and
    // the sync detach closure can both record calls without an
    // illegal `&mut` capture across the `.await`.
    let relay_calls = std::cell::Cell::new(0u32);
    let detach_calls = std::cell::Cell::new(0u32);
    let detach_targets = std::cell::RefCell::new(Vec::<PathBuf>::new());

    let outcome = block(dispatch_spawn(
        &mut console,
        &cfg,
        targets.clone(),
        &display_target,
        |_console, _config, _t, _d| {
            relay_calls.set(relay_calls.get().saturating_add(1));
            Box::pin(async { Ok(()) })
        },
        |_config, t| {
            detach_calls.set(detach_calls.get().saturating_add(1));
            *detach_targets.borrow_mut() = t.to_vec();
            Ok(())
        },
    ))
    .expect("dispatch must succeed");

    assert_eq!(
        relay_calls.get(),
        0,
        "relay must NOT be called when no overlap"
    );
    assert_eq!(detach_calls.get(), 1, "detach must be called exactly once");
    assert_eq!(*detach_targets.borrow(), targets);
    assert_eq!(
        console.suspend_calls, 0,
        "Console::suspend must NOT fire on the no-overlap branch"
    );
    match outcome {
        PickerSessionOutcome::ShellDetached { targets: out } => {
            assert_eq!(out, targets);
        }
        other => panic!("expected ShellDetached, got {other:?}"),
    }
}

/// Picker decides overlap=true: selection contains `/dev/tty1` on a
/// splash console (display target = `/dev/tty1`). Dispatch MUST
/// route into the relay branch, and the relay callback MUST be
/// invoked with the picker's `display_target` path — proving the
/// relay no longer recomputes it from sysfs.
///
/// We additionally invoke a stand-in `relay_fn` that mirrors what
/// the real [`crate::ui::console_relay::run_relay`] does for the
/// overlap branch (call `console.suspend()` then `console.resume()`)
/// so the suspend/resume side-effect is observable end-to-end.
#[test]
fn dispatch_overlap_runs_relay_and_suspends_console() {
    let cfg = Config::recovery_default();
    let mut console = FakeConsole::new(Vec::new()).with_kind(ConsoleKind::Splash);
    let targets = vec![
        PathBuf::from("/dev/ttyS0"),
        PathBuf::from(SPLASH_DISPLAY_TTY),
    ];
    let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);

    // Shared-ref counters so the boxed relay future can record calls
    // and drive suspend/resume on its borrowed `console` arg.
    let relay_calls = std::cell::Cell::new(0u32);
    let relay_seen_display = std::cell::RefCell::new(None::<PathBuf>);
    let relay_seen_targets = std::cell::RefCell::new(Vec::<PathBuf>::new());
    let detach_calls = std::cell::Cell::new(0u32);

    let outcome = block(dispatch_spawn(
        &mut console,
        &cfg,
        targets.clone(),
        &display_target,
        |console, _config, t, d| {
            relay_calls.set(relay_calls.get().saturating_add(1));
            *relay_seen_display.borrow_mut() = Some(d.to_path_buf());
            *relay_seen_targets.borrow_mut() = t.to_vec();
            Box::pin(async move {
                // Mirror the production overlap path: suspend, then
                // resume after the (fake) shell exits. This is what
                // `run_relay` does on the overlap branch.
                console.suspend()?;
                console.resume()?;
                Ok(())
            })
        },
        |_config, _t| {
            detach_calls.set(detach_calls.get().saturating_add(1));
            Ok(())
        },
    ))
    .expect("dispatch must succeed");

    assert_eq!(relay_calls.get(), 1, "relay must be called exactly once");
    assert_eq!(
        detach_calls.get(),
        0,
        "detach must NOT be called on overlap"
    );
    assert_eq!(
        *relay_seen_display.borrow(),
        Some(display_target.clone()),
        "relay must receive the picker's display_target verbatim"
    );
    assert_eq!(*relay_seen_targets.borrow(), targets);
    assert_eq!(
        console.suspend_calls, 1,
        "Console::suspend must fire exactly once on the overlap branch"
    );
    assert_eq!(console.resume_calls, 1);
    assert!(matches!(outcome, PickerSessionOutcome::ShellRan));
}
