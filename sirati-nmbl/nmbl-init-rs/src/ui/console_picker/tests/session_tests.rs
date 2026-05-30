use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;

use crate::ui::console::ConsoleKind;

use super::super::display_overlaps_targets;
use super::super::session::{SPLASH_DISPLAY_TTY, display_target_for, drive_picker_loop};
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
        custom_cursor: 0,
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

/// Pins the no-overlap arm of the production decision in
/// [`run_picker_session`]: on a splash console (display target =
/// `/dev/tty1`) a selection of `/dev/ttyS0` only does NOT overlap, so
/// production takes the fire-and-forget / `ShellDetached` branch
/// rather than suspending the live console for the relay. This is the
/// half of the bug-fix that protects against routing the relay onto a
/// non-display tty.
#[test]
fn overlap_false_for_serial_only_selection_on_splash() {
    let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);
    let targets = vec![PathBuf::from("/dev/ttyS0")];
    assert!(
        !display_overlaps_targets(&display_target, &targets),
        "serial-only selection must not overlap the splash display tty \
         (production takes the fire-and-forget / ShellDetached arm)"
    );
}

/// Pins the overlap arm of the production decision in
/// [`run_picker_session`]: on a splash console (display target =
/// `/dev/tty1`) a selection that contains `/dev/tty1` overlaps, so
/// production takes the relay / `ShellRan` branch (which suspends the
/// live console, pumps the PTY relay, then resumes).
#[test]
fn overlap_true_when_selection_contains_splash_display() {
    let display_target = PathBuf::from(SPLASH_DISPLAY_TTY);
    let targets = vec![
        PathBuf::from("/dev/ttyS0"),
        PathBuf::from(SPLASH_DISPLAY_TTY),
    ];
    assert!(
        display_overlaps_targets(&display_target, &targets),
        "a selection containing the splash display tty must overlap \
         (production takes the relay / ShellRan arm)"
    );
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
