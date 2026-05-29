use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;

use crate::config::Config;
use crate::error::NmblError;
use crate::ui::tty_enum::{EnumeratedTty, TtyKind, is_char_device};

use super::super::state::validate_custom_input;
use super::super::types::{
    ButtonCursor, CandidateOrigin, CustomValidation, FocusZone, PickerCandidate, PickerOutcome,
    PickerState,
};
use super::{no_enum, press};

#[test]
fn build_active_console_is_pre_checked_with_extras() {
    let mut cfg = Config::recovery_default();
    cfg.emergency_shell.extra_consoles = vec!["/dev/ttyS0".to_string(), "/dev/tty1".to_string()];
    let state =
        PickerState::build_with(&cfg, || Ok(PathBuf::from("/dev/tty0")), no_enum).expect("build");
    assert_eq!(state.candidates.len(), 3);
    assert_eq!(state.candidates[0].target, PathBuf::from("/dev/tty0"));
    assert!(state.candidates[0].is_active());
    assert!(state.selected[0]);
    assert_eq!(state.candidates[1].target, PathBuf::from("/dev/ttyS0"));
    assert!(!state.selected[1]);
    assert_eq!(state.candidates[2].target, PathBuf::from("/dev/tty1"));
    assert!(!state.selected[2]);
}

#[test]
fn build_merges_enumerated_set_after_kernel_console() {
    let cfg = Config::recovery_default();
    let state = PickerState::build_with(
        &cfg,
        || Ok(PathBuf::from("/dev/ttyS0")),
        |exclude| {
            vec![
                EnumeratedTty {
                    path: PathBuf::from("/dev/tty1"),
                    kind: TtyKind::FramebufferTty,
                },
                // ttyS0 must be filtered out by the enumerator
                // because it equals `exclude`; assert the contract.
                EnumeratedTty {
                    path: if exclude == Path::new("/dev/ttyS0") {
                        PathBuf::from("/dev/ttyS1")
                    } else {
                        PathBuf::from("/dev/ttyS0")
                    },
                    kind: TtyKind::SerialPort,
                },
            ]
        },
    )
    .expect("build");
    assert_eq!(state.candidates.len(), 3);
    assert_eq!(state.candidates[0].target, PathBuf::from("/dev/ttyS0"));
    assert!(state.candidates[0].is_active());
    assert_eq!(state.candidates[1].target, PathBuf::from("/dev/tty1"));
    assert!(matches!(
        state.candidates[1].origin,
        CandidateOrigin::Enumerated(TtyKind::FramebufferTty)
    ));
    assert_eq!(state.candidates[2].target, PathBuf::from("/dev/ttyS1"));
}

#[test]
fn build_deduplicates_extra_that_matches_enumerated_entry() {
    let mut cfg = Config::recovery_default();
    cfg.emergency_shell.extra_consoles = vec!["/dev/tty1".to_string()];
    let state = PickerState::build_with(
        &cfg,
        || Ok(PathBuf::from("/dev/tty0")),
        |_| {
            vec![EnumeratedTty {
                path: PathBuf::from("/dev/tty1"),
                kind: TtyKind::FramebufferTty,
            }]
        },
    )
    .expect("build");
    assert_eq!(state.candidates.len(), 2);
    assert_eq!(state.candidates[0].target, PathBuf::from("/dev/tty0"));
    assert_eq!(state.candidates[1].target, PathBuf::from("/dev/tty1"));
}

#[test]
fn build_falls_back_to_dev_console_when_resolver_errors() {
    let cfg = Config::recovery_default();
    let state = PickerState::build_with(
        &cfg,
        || {
            Err(NmblError::Tui {
                source: std::io::Error::other("no /sys"),
            })
        },
        no_enum,
    )
    .expect("fallback build must still succeed");
    assert_eq!(state.candidates.len(), 1);
    assert_eq!(state.candidates[0].target, PathBuf::from("/dev/console"));
    assert!(state.candidates[0].is_active());
}

#[test]
fn space_toggles_checkbox_on_focused_row() {
    let mut state = PickerState {
        candidates: vec![
            PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            },
            PickerCandidate {
                label: "b".into(),
                target: PathBuf::from("/dev/b"),
                origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
            },
        ],
        selected: vec![true, false],
        cursor: 1,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    assert!(!state.on_key(press(KeyCode::Char(' '))));
    assert!(state.selected[1]);
    assert!(!state.on_key(press(KeyCode::Char(' '))));
    assert!(!state.selected[1]);
}

#[test]
fn spawn_enter_returns_selected_targets() {
    let mut state = PickerState {
        candidates: vec![
            PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            },
            PickerCandidate {
                label: "b".into(),
                target: PathBuf::from("/dev/b"),
                origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
            },
        ],
        selected: vec![true, true],
        // candidates(2) + custom-input(1) → buttons starts at 3.
        cursor: 3,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    assert!(state.on_key(press(KeyCode::Enter)));
    match state.outcome {
        Some(PickerOutcome::Spawn { targets }) => {
            assert_eq!(
                targets,
                vec![PathBuf::from("/dev/a"), PathBuf::from("/dev/b")]
            );
        }
        other => panic!("expected Spawn, got {other:?}"),
    }
}

#[test]
fn spawn_with_nothing_selected_is_rejected() {
    let mut state = PickerState {
        candidates: vec![PickerCandidate {
            label: "a".into(),
            target: PathBuf::from("/dev/a"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![false],
        cursor: 2,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    assert!(!state.on_key(press(KeyCode::Enter)));
    assert!(state.outcome.is_none());
}

#[test]
fn esc_commits_cancel() {
    let mut state = PickerState {
        candidates: vec![PickerCandidate {
            label: "a".into(),
            target: PathBuf::from("/dev/a"),
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
    assert!(state.on_key(press(KeyCode::Esc)));
    assert!(matches!(state.outcome, Some(PickerOutcome::Cancel)));
}

#[test]
fn cursor_navigation_walks_list_custom_buttons() {
    let mut state = PickerState {
        candidates: vec![
            PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                origin: CandidateOrigin::KernelConsole,
            },
            PickerCandidate {
                label: "b".into(),
                target: PathBuf::from("/dev/b"),
                origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
            },
        ],
        selected: vec![true, false],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    state.on_key(press(KeyCode::Down));
    assert_eq!(state.focus(), FocusZone::List);
    state.on_key(press(KeyCode::Down));
    assert_eq!(state.focus(), FocusZone::CustomInput);
    state.on_key(press(KeyCode::Down));
    assert_eq!(state.focus(), FocusZone::Buttons);
    state.on_key(press(KeyCode::Down));
    // saturates on buttons
    assert_eq!(state.focus(), FocusZone::Buttons);
    state.on_key(press(KeyCode::Right));
    assert_eq!(state.button_cursor, ButtonCursor::Cancel);
}

/// Custom-input validation: empty → Empty; invalid path → Invalid;
/// real chardev → Valid. We use `/dev/null` because it is a chardev
/// guaranteed to exist in every NMBL target environment.
#[test]
fn custom_input_validation_empty_invalid_valid() {
    let existing = vec![PickerCandidate {
        label: "kernel".into(),
        target: PathBuf::from("/dev/console"),
        origin: CandidateOrigin::KernelConsole,
    }];
    assert_eq!(
        validate_custom_input("", &existing),
        CustomValidation::Empty
    );
    assert_eq!(
        validate_custom_input("   ", &existing),
        CustomValidation::Empty
    );
    assert_eq!(
        validate_custom_input("/etc/passwd", &existing),
        CustomValidation::Invalid,
        "non /dev/ paths must be rejected"
    );
    assert_eq!(
        validate_custom_input("/dev/", &existing),
        CustomValidation::Invalid,
        "/dev/ with no name must be rejected"
    );
    assert_eq!(
        validate_custom_input("/dev/this-should-not-exist-nmbl", &existing),
        CustomValidation::Invalid,
        "missing devnode must be rejected"
    );
    assert_eq!(
        validate_custom_input("/dev/console", &existing),
        CustomValidation::Invalid,
        "duplicates of existing candidates must be rejected"
    );
    if is_char_device(Path::new("/dev/null")) {
        assert_eq!(
            validate_custom_input("/dev/null", &existing),
            CustomValidation::Valid,
            "an existing chardev not in the list must validate"
        );
    }
}

#[test]
fn typing_into_custom_input_updates_buffer() {
    let mut state = PickerState {
        candidates: vec![PickerCandidate {
            label: "a".into(),
            target: PathBuf::from("/dev/a"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        // jump straight to the custom-input row
        cursor: 1,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    for c in "/dev/ttyS9".chars() {
        state.on_key(press(KeyCode::Char(c)));
    }
    assert_eq!(state.custom_input, "/dev/ttyS9");
    assert_eq!(state.custom_cursor, "/dev/ttyS9".len());
    state.on_key(press(KeyCode::Backspace));
    assert_eq!(state.custom_input, "/dev/ttyS");
    assert_eq!(state.custom_cursor, "/dev/ttyS".len());
}

#[test]
fn valid_custom_input_appears_in_selected_targets() {
    if !is_char_device(Path::new("/dev/null")) {
        return;
    }
    let state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: "/dev/null".to_string(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    let targets = state.selected_targets();
    assert!(targets.contains(&PathBuf::from("/dev/tty0")));
    assert!(targets.contains(&PathBuf::from("/dev/null")));
}

#[test]
fn invalid_custom_input_is_excluded_from_selected_targets() {
    let state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: "/dev/does-not-exist".to_string(),
        custom_cursor: 0,
        custom_checked: true,
        outcome: None,
    };
    let targets = state.selected_targets();
    assert_eq!(targets, vec![PathBuf::from("/dev/tty0")]);
}
