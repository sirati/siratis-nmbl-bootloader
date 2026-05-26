//! TUI state machine. Pure logic: takes `crossterm::event::KeyEvent`s
//! and mutates [`App`]; the surrounding `ui::mod` is responsible for
//! actually polling input and rendering frames via [`crate::ui::view`].
//!
//! The state machine has three screens:
//! - [`Screen::List`]    — generation picker, default landing page.
//! - [`Screen::Editing`] — single-line kernel-cmdline editor.
//! - [`Screen::Passphrase`] — modal LUKS prompt driven by activation.rs.
//!
//! When the user makes a final decision the `decision` field is set
//! and [`App::on_key`] returns `true`, signalling the run loop to exit.
//! The passphrase modal is the exception: Enter on a passphrase screen
//! leaves the App alive (the caller — [`crate::ui::TuiPasswordSupplier`]
//! — drains the buffer and returns it without exiting the App), and
//! only Esc on the passphrase modal sets a [`Decision::Shell`] exit.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use zeroize::Zeroizing;

use crate::generations::Generation;

/// Top-level user choice returned when the TUI exits.
#[derive(Debug)]
pub enum Decision {
    /// User chose to boot this generation. cmdline may have been
    /// edited in the TUI.
    Boot {
        generation_index: usize,
        cmdline_override: Option<String>,
    },
    /// User asked for the emergency shell.
    Shell,
    /// User asked to reboot the machine (not common but useful).
    Reboot,
}

/// Which screen the App is currently presenting.
pub enum Screen {
    List,
    Editing {
        /// Index into the generations slice.
        generation_index: usize,
        /// Working buffer for the cmdline.
        buffer: String,
        /// Byte index into `buffer`; always lies on a char boundary.
        cursor: usize,
    },
    Passphrase {
        prompt_label: String,
        buffer: Zeroizing<String>,
        error_message: Option<String>,
    },
}

/// Top-level TUI app state.
pub struct App<'a> {
    pub generations: &'a [Generation],
    pub selected_index: usize,
    pub screen: Screen,
    pub show_kernel_params: bool,
    pub countdown_remaining_secs: Option<u64>,
    pub decision: Option<Decision>,
}

impl<'a> App<'a> {
    pub fn new(generations: &'a [Generation]) -> Self {
        Self {
            generations,
            selected_index: 0,
            screen: Screen::List,
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
        }
    }

    /// Reduce a crossterm KeyEvent into a state mutation. Returns
    /// `true` if the App wants to exit (decision is Some).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ignore Release/Repeat so a held key doesn't fire repeatedly
        // and a key-up after the decisive Press doesn't re-trigger.
        if key.kind != KeyEventKind::Press {
            return self.decision.is_some();
        }

        // Any keypress cancels the countdown — even one we ignore later.
        self.countdown_remaining_secs = None;

        match &mut self.screen {
            Screen::List => Self::handle_list_key(
                key.code,
                &mut self.selected_index,
                self.generations,
                &mut self.screen,
                &mut self.show_kernel_params,
                &mut self.decision,
            ),
            Screen::Editing { .. } => Self::handle_editing_key(
                key.code,
                &mut self.screen,
                &mut self.decision,
            ),
            Screen::Passphrase { .. } => Self::handle_passphrase_key(
                key.code,
                &mut self.screen,
                &mut self.decision,
            ),
        }
    }

    fn handle_list_key(
        code: KeyCode,
        selected_index: &mut usize,
        generations: &[Generation],
        screen: &mut Screen,
        show_kernel_params: &mut bool,
        decision: &mut Option<Decision>,
    ) -> bool {
        let last_idx = generations.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected_index = selected_index.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected_index < last_idx {
                    *selected_index = selected_index.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                *decision = Some(Decision::Boot {
                    generation_index: *selected_index,
                    cmdline_override: None,
                });
                true
            }
            KeyCode::Char('e') => {
                let buffer = generations
                    .get(*selected_index)
                    .map(|g| g.kernel_params.join(" "))
                    .unwrap_or_default();
                let cursor = buffer.len();
                *screen = Screen::Editing {
                    generation_index: *selected_index,
                    buffer,
                    cursor,
                };
                false
            }
            KeyCode::Char('p') => {
                *show_kernel_params = !*show_kernel_params;
                false
            }
            KeyCode::Char('s') => {
                *decision = Some(Decision::Shell);
                true
            }
            KeyCode::Char('q') => {
                *decision = Some(Decision::Reboot);
                true
            }
            _ => false,
        }
    }

    fn handle_editing_key(
        code: KeyCode,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Editing {
            generation_index,
            buffer,
            cursor,
        } = screen
        else {
            return false;
        };

        match code {
            KeyCode::Char(c) => {
                let insert_at = clamp_to_char_boundary(buffer, *cursor);
                buffer.insert(insert_at, c);
                *cursor = insert_at.saturating_add(c.len_utf8());
                false
            }
            KeyCode::Backspace => {
                let current = clamp_to_char_boundary(buffer, *cursor);
                if let Some(prev) = prev_char_boundary(buffer, current) {
                    buffer.replace_range(prev..current, "");
                    *cursor = prev;
                }
                false
            }
            KeyCode::Left => {
                let current = clamp_to_char_boundary(buffer, *cursor);
                if let Some(prev) = prev_char_boundary(buffer, current) {
                    *cursor = prev;
                } else {
                    *cursor = 0;
                }
                false
            }
            KeyCode::Right => {
                let current = clamp_to_char_boundary(buffer, *cursor);
                *cursor = next_char_boundary(buffer, current).unwrap_or(buffer.len());
                false
            }
            KeyCode::Home => {
                *cursor = 0;
                false
            }
            KeyCode::End => {
                *cursor = buffer.len();
                false
            }
            KeyCode::Enter => {
                *decision = Some(Decision::Boot {
                    generation_index: *generation_index,
                    cmdline_override: Some(buffer.clone()),
                });
                true
            }
            KeyCode::Esc => {
                *screen = Screen::List;
                false
            }
            _ => false,
        }
    }

    fn handle_passphrase_key(
        code: KeyCode,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Passphrase { buffer, .. } = screen else {
            return false;
        };

        match code {
            KeyCode::Char(c) => {
                buffer.push(c);
                false
            }
            KeyCode::Backspace => {
                buffer.pop();
                false
            }
            KeyCode::Enter => {
                // Caller (TuiPasswordSupplier) detects the buffer is
                // ready by polling — we do NOT exit the App here.
                // Signal "consumed" with `true` so the supplier's
                // dispatch loop can return cleanly.
                true
            }
            KeyCode::Esc => {
                *decision = Some(Decision::Shell);
                true
            }
            _ => false,
        }
    }
}

/// Round `byte_idx` down to the nearest char boundary in `s`. The
/// editor stores the cursor as a byte index and the screen renders
/// it as a char count, so we have to be precise about boundaries.
fn clamp_to_char_boundary(s: &str, byte_idx: usize) -> usize {
    let len = s.len();
    if byte_idx >= len {
        return len;
    }
    let mut idx = byte_idx;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    idx
}

/// Byte index of the char boundary strictly before `byte_idx`, or
/// `None` if `byte_idx` is at the start (or before).
fn prev_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    if byte_idx == 0 {
        return None;
    }
    let mut idx = byte_idx.saturating_sub(1);
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    Some(idx)
}

/// Byte index of the next char boundary after `byte_idx`, or `None`
/// if `byte_idx` is at or past the end.
fn next_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    let len = s.len();
    if byte_idx >= len {
        return None;
    }
    let mut idx = byte_idx.saturating_add(1);
    while idx < len && !s.is_char_boundary(idx) {
        idx = idx.saturating_add(1);
    }
    Some(idx)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn fake_gen(number: u32, params: &[&str]) -> Generation {
        Generation {
            number,
            profile_link: PathBuf::from(format!("/p/system-{number}-link")),
            kernel: PathBuf::from("/p/kernel"),
            initrd: PathBuf::from("/p/initrd"),
            kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
            label: String::new(),
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn list_arrow_keys_move_selection_within_bounds() {
        let gens = vec![fake_gen(3, &[]), fake_gen(2, &[]), fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert_eq!(app.selected_index, 0);

        // Up at index 0 stays at 0.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(app.selected_index, 0);

        // Down moves through the list.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 1);
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 2);

        // Down at end stays at end.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 2);

        // vi-keys also work.
        assert!(!app.on_key(press(KeyCode::Char('k'))));
        assert_eq!(app.selected_index, 1);
        assert!(!app.on_key(press(KeyCode::Char('j'))));
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn list_e_transitions_to_editing_with_joined_params() {
        let gens = vec![fake_gen(42, &["init=/sbin/init", "quiet", "loglevel=4"])];
        let mut app = App::new(&gens);

        assert!(!app.on_key(press(KeyCode::Char('e'))));
        match &app.screen {
            Screen::Editing {
                generation_index,
                buffer,
                cursor,
            } => {
                assert_eq!(*generation_index, 0);
                assert_eq!(buffer, "init=/sbin/init quiet loglevel=4");
                assert_eq!(*cursor, buffer.len(), "cursor must land at end");
            }
            _ => panic!("expected Editing screen"),
        }
    }

    #[test]
    fn list_s_sets_shell_decision_and_returns_true() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(app.on_key(press(KeyCode::Char('s'))));
        assert!(matches!(app.decision, Some(Decision::Shell)));
    }

    #[test]
    fn list_q_sets_reboot_decision_and_returns_true() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(app.on_key(press(KeyCode::Char('q'))));
        assert!(matches!(app.decision, Some(Decision::Reboot)));
    }

    #[test]
    fn list_enter_sets_boot_decision_with_no_override() {
        let gens = vec![fake_gen(7, &[]), fake_gen(6, &[])];
        let mut app = App::new(&gens);
        app.selected_index = 1;
        assert!(app.on_key(press(KeyCode::Enter)));
        match &app.decision {
            Some(Decision::Boot {
                generation_index,
                cmdline_override,
            }) => {
                assert_eq!(*generation_index, 1);
                assert!(cmdline_override.is_none());
            }
            other => panic!("expected Boot decision, got {other:?}"),
        }
    }

    #[test]
    fn list_p_toggles_show_kernel_params() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(!app.show_kernel_params);
        app.on_key(press(KeyCode::Char('p')));
        assert!(app.show_kernel_params);
        app.on_key(press(KeyCode::Char('p')));
        assert!(!app.show_kernel_params);
    }

    #[test]
    fn any_keypress_in_list_cancels_countdown() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        app.countdown_remaining_secs = Some(4);
        // 'p' is a no-op-ish toggle, but should still clear the countdown.
        app.on_key(press(KeyCode::Char('p')));
        assert!(app.countdown_remaining_secs.is_none());
    }

    #[test]
    fn editing_typing_appends_and_backspace_removes() {
        let gens = vec![fake_gen(1, &["foo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));

        // Append " bar".
        for c in " bar".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        match &app.screen {
            Screen::Editing { buffer, cursor, .. } => {
                assert_eq!(buffer, "foo bar");
                assert_eq!(*cursor, buffer.len());
            }
            _ => panic!("expected Editing"),
        }

        // Backspace once removes 'r'.
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Editing { buffer, .. } => assert_eq!(buffer, "foo ba"),
            _ => panic!("expected Editing"),
        }
    }

    #[test]
    fn editing_enter_sets_boot_with_cmdline_override() {
        let gens = vec![fake_gen(5, &["root=/dev/sda1"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        for c in " quiet".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        assert!(app.on_key(press(KeyCode::Enter)));
        match &app.decision {
            Some(Decision::Boot {
                generation_index,
                cmdline_override,
            }) => {
                assert_eq!(*generation_index, 0);
                assert_eq!(cmdline_override.as_deref(), Some("root=/dev/sda1 quiet"));
            }
            other => panic!("expected Boot{{..}}, got {other:?}"),
        }
    }

    #[test]
    fn editing_esc_returns_to_list_without_decision() {
        let gens = vec![fake_gen(5, &["foo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        assert!(matches!(app.screen, Screen::Editing { .. }));
        assert!(!app.on_key(press(KeyCode::Esc)));
        assert!(matches!(app.screen, Screen::List));
        assert!(app.decision.is_none());
    }

    #[test]
    fn editing_home_end_left_right_navigation() {
        let gens = vec![fake_gen(1, &["abcd"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));

        // Cursor starts at end. Home jumps to 0.
        app.on_key(press(KeyCode::Home));
        match &app.screen {
            Screen::Editing { cursor, .. } => assert_eq!(*cursor, 0),
            _ => panic!(),
        }
        // Right advances one byte.
        app.on_key(press(KeyCode::Right));
        match &app.screen {
            Screen::Editing { cursor, .. } => assert_eq!(*cursor, 1),
            _ => panic!(),
        }
        // End jumps to the end.
        app.on_key(press(KeyCode::End));
        match &app.screen {
            Screen::Editing { cursor, buffer, .. } => assert_eq!(*cursor, buffer.len()),
            _ => panic!(),
        }
        // Left walks back one byte.
        app.on_key(press(KeyCode::Left));
        match &app.screen {
            Screen::Editing { cursor, buffer, .. } => {
                assert_eq!(*cursor, buffer.len().saturating_sub(1));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn editing_handles_multibyte_backspace_without_panic() {
        // Backspacing across a multi-byte char boundary must not panic
        // even though clippy's indexing_slicing lint applies to prod code.
        let gens = vec![fake_gen(1, &["héllo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Editing { buffer, cursor, .. } => {
                assert_eq!(buffer, "héll");
                assert_eq!(*cursor, buffer.len());
            }
            _ => panic!("expected Editing"),
        }
    }

    #[test]
    fn passphrase_screen_collects_chars_and_pops() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            error_message: None,
        };
        for c in "hi".chars() {
            assert!(!app.on_key(press(KeyCode::Char(c))));
        }
        match &app.screen {
            Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "hi"),
            _ => panic!(),
        }
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "h"),
            _ => panic!(),
        }
    }

    #[test]
    fn passphrase_esc_drops_to_shell() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            error_message: None,
        };
        assert!(app.on_key(press(KeyCode::Esc)));
        assert!(matches!(app.decision, Some(Decision::Shell)));
    }

    #[test]
    fn passphrase_enter_signals_consumed_without_decision() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new("secret".to_string()),
            error_message: None,
        };
        assert!(app.on_key(press(KeyCode::Enter)));
        assert!(app.decision.is_none(), "Enter must not set a Decision");
    }

    #[test]
    fn release_events_are_ignored() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(!app.on_key(release));
        assert!(app.decision.is_none());
    }
}
