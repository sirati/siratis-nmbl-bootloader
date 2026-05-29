use crossterm::event::{KeyCode, KeyEvent};

use crate::error::Result;
use crate::splash::drm;
use crate::splash::glyph_cache;
use crate::splash::input::SplashInput;
use crate::splash::types::CellDims;
use crate::ui::POLL_SLICE;

use super::render::render_frame;
use super::{DemoOutcome, DemoState, MAX_ATTEMPTS, StepResult};

/// Run the demo loop. Blocks until the operator picks Shell or Reboot
/// from the emergency menu.
pub fn run(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    input: &mut SplashInput,
) -> Result<DemoOutcome> {
    let mut state = DemoState::Entering {
        buffer: String::new(),
        attempts: 0,
    };
    let mut dirty = true;
    loop {
        if dirty {
            render_frame(drm, bg_scaled, cache, cell_dims, &state)?;
            dirty = false;
        }
        if let Some(key) = input.poll(POLL_SLICE)? {
            match step(&mut state, key) {
                StepResult::Continue => dirty = true,
                StepResult::Done(o) => return Ok(o),
            }
        }
    }
}

/// Fold one key press into the state machine. Pure — no IO. Tests
/// drive this directly with synthetic `KeyEvent`s.
pub(super) fn step(state: &mut DemoState, key: KeyEvent) -> StepResult {
    match state {
        DemoState::Entering { buffer, attempts } => {
            match key.code {
                KeyCode::Char(c) => buffer.push(c),
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Enter => {
                    *attempts = attempts.saturating_add(1);
                    buffer.clear();
                    if *attempts >= MAX_ATTEMPTS {
                        *state = DemoState::Emergency {
                            selected: 0,
                            attempts: *attempts,
                        };
                    }
                }
                KeyCode::Esc => {
                    *state = DemoState::Emergency {
                        selected: 0,
                        attempts: *attempts,
                    };
                }
                _ => {}
            }
            StepResult::Continue
        }
        DemoState::Emergency { selected, attempts } => match key.code {
            KeyCode::Up => {
                *selected = (*selected).saturating_sub(1);
                StepResult::Continue
            }
            KeyCode::Down => {
                if *selected < 2 {
                    *selected = selected.saturating_add(1);
                }
                StepResult::Continue
            }
            KeyCode::Enter => match *selected {
                0 => {
                    *state = DemoState::Entering {
                        buffer: String::new(),
                        attempts: 0,
                    };
                    StepResult::Continue
                }
                1 => StepResult::Done(DemoOutcome::DroppedToShell),
                _ => StepResult::Done(DemoOutcome::RebootRequested),
            },
            KeyCode::Esc => {
                // Esc from the emergency menu returns to the entry
                // screen preserving the current attempt counter.
                *state = DemoState::Entering {
                    buffer: String::new(),
                    attempts: *attempts,
                };
                StepResult::Continue
            }
            _ => StepResult::Continue,
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::StepResult;
    use super::super::{DemoOutcome, DemoState, MAX_ATTEMPTS};
    use super::step;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn drive(state: &mut DemoState, codes: &[KeyCode]) -> Option<DemoOutcome> {
        for code in codes {
            match step(state, press(*code)) {
                StepResult::Continue => {}
                StepResult::Done(o) => return Some(o),
            }
        }
        None
    }

    #[test]
    fn typing_then_enter_increments_attempts_and_clears_buffer() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        assert!(drive(&mut state, &[KeyCode::Char('h'), KeyCode::Char('i')]).is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert_eq!(buffer, "hi");
                assert_eq!(*attempts, 0);
            }
            _ => panic!("expected Entering"),
        }
        assert!(drive(&mut state, &[KeyCode::Enter]).is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert!(buffer.is_empty(), "buffer must clear after Enter");
                assert_eq!(*attempts, 1);
            }
            _ => panic!("expected Entering"),
        }
    }

    #[test]
    fn backspace_pops_one_char_from_buffer() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(
            &mut state,
            &[KeyCode::Char('a'), KeyCode::Char('b'), KeyCode::Backspace],
        );
        match &state {
            DemoState::Entering { buffer, .. } => assert_eq!(buffer, "a"),
            _ => panic!(),
        }
    }

    #[test]
    fn three_failed_attempts_transitions_to_emergency() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(
            &mut state,
            &[KeyCode::Enter, KeyCode::Enter, KeyCode::Enter],
        );
        assert!(matches!(
            state,
            DemoState::Emergency {
                selected: 0,
                attempts: 3,
            }
        ));
    }

    #[test]
    fn esc_from_entering_jumps_to_emergency_immediately() {
        let mut state = DemoState::Entering {
            buffer: "halfway".to_string(),
            attempts: 1,
        };
        drive(&mut state, &[KeyCode::Esc]);
        assert!(matches!(
            state,
            DemoState::Emergency {
                selected: 0,
                attempts: 1,
            }
        ));
    }

    #[test]
    fn emergency_arrow_keys_navigate_within_bounds() {
        let mut state = DemoState::Emergency {
            selected: 0,
            attempts: MAX_ATTEMPTS,
        };
        drive(&mut state, &[KeyCode::Up]);
        assert!(matches!(state, DemoState::Emergency { selected: 0, .. }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 1, .. }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 2, .. }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 2, .. }));
        drive(&mut state, &[KeyCode::Up]);
        assert!(matches!(state, DemoState::Emergency { selected: 1, .. }));
    }

    #[test]
    fn emergency_enter_on_retry_resets_to_entering() {
        let mut state = DemoState::Emergency {
            selected: 0,
            attempts: MAX_ATTEMPTS,
        };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert!(out.is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert!(buffer.is_empty());
                assert_eq!(*attempts, 0);
            }
            _ => panic!("expected Entering after retry"),
        }
    }

    #[test]
    fn emergency_enter_on_shell_returns_dropped_to_shell() {
        let mut state = DemoState::Emergency {
            selected: 1,
            attempts: MAX_ATTEMPTS,
        };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::DroppedToShell));
    }

    #[test]
    fn emergency_enter_on_reboot_returns_reboot_requested() {
        let mut state = DemoState::Emergency {
            selected: 2,
            attempts: MAX_ATTEMPTS,
        };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::RebootRequested));
    }

    #[test]
    fn emergency_esc_preserves_attempts_when_returning_to_entering() {
        let mut state = DemoState::Emergency {
            selected: 1,
            attempts: 1,
        };
        drive(&mut state, &[KeyCode::Esc]);
        match &state {
            DemoState::Entering { attempts, buffer } => {
                assert_eq!(*attempts, 1);
                assert!(buffer.is_empty());
            }
            _ => panic!("expected Entering"),
        }
    }

    #[test]
    fn full_flow_three_fails_then_pick_reboot() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(&mut state, &[KeyCode::Char('x'), KeyCode::Enter]);
        drive(&mut state, &[KeyCode::Char('y'), KeyCode::Enter]);
        drive(&mut state, &[KeyCode::Char('z'), KeyCode::Enter]);
        assert!(matches!(state, DemoState::Emergency { .. }));
        drive(&mut state, &[KeyCode::Down, KeyCode::Down]);
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::RebootRequested));
    }

    #[test]
    fn cancelled_outcome_is_a_distinct_variant() {
        // Pin the public-API surface: Cancelled is exported so callers
        // can match on it without falling through.
        let outcomes = [
            DemoOutcome::RetryRequested,
            DemoOutcome::DroppedToShell,
            DemoOutcome::RebootRequested,
            DemoOutcome::Cancelled,
        ];
        assert_eq!(outcomes.len(), 4);
    }
}
