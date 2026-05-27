//! TUI state machine. Pure logic: takes `crossterm::event::KeyEvent`s
//! and mutates [`App`]; the surrounding `ui::mod` is responsible for
//! actually polling input and rendering frames via [`crate::ui::view`].
//!
//! The state machine has five screens:
//! - [`Screen::List`]    — generation picker, default landing page.
//! - [`Screen::Editing`] — single-line kernel-cmdline editor.
//! - [`Screen::Passphrase`] — modal LUKS prompt driven by activation.rs.
//! - [`Screen::Emergency`] — boot-failed picker between Reboot and Shell.
//! - [`Screen::BootStatus`] — non-interactive progress + log view shown
//!   during early boot phases (before the selector / activation).
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

/// Choice the operator can make on the emergency screen.
///
/// Kept separate from [`Decision`] because the boot-menu Decision
/// machinery is geared around generations + cmdline overrides, which
/// the emergency screen has no business expressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmergencyChoice {
    /// Reboot the machine via `reboot(RB_AUTOBOOT)`.
    Reboot,
    /// Drop to the configured emergency shell.
    Shell,
}

/// One row on the emergency screen.
pub struct EmergencyItem {
    pub label: &'static str,
    pub choice: EmergencyChoice,
}

/// Per-frame snapshot shown by [`Screen::BootStatus`].
///
/// Owned by the App so callers can mutate fields between frames via the
/// `set_*` / `tick_*` helpers on [`App`]; the renderer is purely a
/// consumer of this struct.
pub struct BootStatusData<'a> {
    /// Current phase label, e.g. "phase 3: storage activations" or
    /// "waiting for /dev/disk/by-uuid/X (12s/30s)".
    pub phase: std::borrow::Cow<'a, str>,
    /// Snapshot of the recent log lines (already gathered by caller).
    /// Most recent last; the renderer clips to the visible panel.
    pub log_lines: Vec<String>,
    /// Spinner phase. Caller increments via [`App::tick_boot_spinner`];
    /// renderer maps to a glyph by `spinner_frame % SPINNER_FRAMES`.
    pub spinner_frame: u8,
}

/// Which screen the App is currently presenting.
pub enum Screen<'a> {
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
    },
    /// Boot has failed. Show the error and let the operator pick
    /// between Reboot and Shell. Defaults are owned by the caller —
    /// the screen just runs the picker.
    Emergency {
        /// Human-readable explanation (already formatted error chain).
        message: String,
        /// Items to display, in order. The screen renders them as a
        /// list and lets the operator pick one.
        items: Vec<EmergencyItem>,
        /// Selected row index, clamped to `items.len() - 1` on render.
        selected: usize,
        /// Final choice the operator committed to; `None` until Enter.
        chosen: Option<EmergencyChoice>,
    },
    /// Non-interactive progress view shown during early boot. The
    /// caller drives the phase label, log snapshot, and spinner tick;
    /// key events are absorbed but never produce a [`Decision`].
    BootStatus(BootStatusData<'a>),
}

/// Top-level TUI app state.
pub struct App<'a> {
    pub generations: &'a [Generation],
    pub selected_index: usize,
    pub screen: Screen<'a>,
    pub show_kernel_params: bool,
    pub countdown_remaining_secs: Option<u64>,
    pub decision: Option<Decision>,
}

/// Number of frames in the boot-status spinner cycle.
///
/// We deliberately use the 4-frame ASCII rotor `|/-\` rather than the
/// 10-frame braille systemd uses. The splash glyph cache (see
/// `src/splash/glyph_cache.rs`) only rasterises ASCII printable plus
/// the box-drawing subset ratatui uses for borders; Unicode braille
/// (U+2800 block) is not in the cache, so `cache.get(c, _)` would
/// return `None` and the splash compositor would draw nothing. On a
/// crossterm terminal the braille would render fine, but the boot
/// screen needs to look identical on both backends — pick ASCII for
/// guaranteed coverage.
pub const SPINNER_FRAMES: u8 = 4;

/// The ASCII spinner glyph sequence. Indexed by `spinner_frame % SPINNER_FRAMES`.
pub const SPINNER_GLYPHS: [char; SPINNER_FRAMES as usize] = ['|', '/', '-', '\\'];

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

    /// Construct an App parked on the [`Screen::BootStatus`] view with
    /// the given phase label, an empty log buffer, and spinner_frame=0.
    ///
    /// `generations` is empty because the boot-status screen runs
    /// before the selector has anything to show. A future caller can
    /// transition out of the boot-status screen by replacing
    /// `self.screen` directly.
    pub fn boot_status(phase: impl Into<std::borrow::Cow<'a, str>>) -> App<'a> {
        App {
            generations: &[],
            selected_index: 0,
            screen: Screen::BootStatus(BootStatusData {
                phase: phase.into(),
                log_lines: Vec::new(),
                spinner_frame: 0,
            }),
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
        }
    }

    /// Replace the phase label of the boot-status screen. No-op when
    /// the App is on any other screen so a stray phase update from a
    /// late-firing supervisor task can't crash production.
    pub fn set_boot_phase(&mut self, phase: impl Into<std::borrow::Cow<'a, str>>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.phase = phase.into();
        } else {
            debug_assert!(false, "set_boot_phase called on non-BootStatus screen");
        }
    }

    /// Replace the log-line snapshot. The caller (typically holding a
    /// log-ring snapshot via `crate::log::snapshot`) is responsible for
    /// ordering: most recent last.
    pub fn set_boot_log_lines(&mut self, lines: Vec<String>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.log_lines = lines;
        } else {
            debug_assert!(false, "set_boot_log_lines called on non-BootStatus screen");
        }
    }

    /// Advance the spinner one frame. Wraps modulo [`SPINNER_FRAMES`]
    /// so callers can tick on any interval without checking the count.
    pub fn tick_boot_spinner(&mut self) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.spinner_frame = data.spinner_frame.wrapping_add(1) % SPINNER_FRAMES;
        } else {
            debug_assert!(false, "tick_boot_spinner called on non-BootStatus screen");
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
            Screen::Editing { .. } => {
                Self::handle_editing_key(key.code, &mut self.screen, &mut self.decision)
            }
            Screen::Passphrase { .. } => {
                Self::handle_passphrase_key(key.code, &mut self.screen, &mut self.decision)
            }
            Screen::Emergency { .. } => Self::handle_emergency_key(key.code, &mut self.screen),
            // BootStatus absorbs keypresses without producing a Decision.
            // The boot-status screen is non-interactive: it shows progress
            // until the caller flips the App to a different screen.
            Screen::BootStatus(_) => false,
        }
    }

    fn handle_emergency_key(code: KeyCode, screen: &mut Screen) -> bool {
        let Screen::Emergency {
            items,
            selected,
            chosen,
            ..
        } = screen
        else {
            return false;
        };

        let last_idx = items.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected < last_idx {
                    *selected = selected.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                if let Some(item) = items.get(*selected) {
                    *chosen = Some(item.choice);
                    true
                } else {
                    false
                }
            }
            // Hotkeys: 'r' for reboot, 's' for shell. Operators in a
            // boot-failure scenario tend to be muscle-memory typing one
            // of those two letters.
            KeyCode::Char('r') => {
                *chosen = Some(EmergencyChoice::Reboot);
                true
            }
            KeyCode::Char('s') => {
                *chosen = Some(EmergencyChoice::Shell);
                true
            }
            KeyCode::Esc => {
                // Esc is a no-op: it preserves the prior selection so a
                // stray keypress doesn't commit. The caller can decide
                // separately to fall through to the default on timeout.
                false
            }
            _ => false,
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
                // Guard against an empty list: emitting a Boot
                // decision with index 0 would crash the caller as
                // soon as it tried to look up the generation.
                if generations.is_empty() {
                    return false;
                }
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
            init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
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
    fn list_enter_with_empty_generations_does_not_decide() {
        // Defence-in-depth: if the selector ever ran with zero
        // generations, Enter would otherwise emit Boot{0,..} and
        // main.rs would index out of bounds. Make Enter a no-op.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        assert!(!app.on_key(press(KeyCode::Enter)));
        assert!(app.decision.is_none(), "decision must stay None");
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
        };
        assert!(app.on_key(press(KeyCode::Enter)));
        assert!(app.decision.is_none(), "Enter must not set a Decision");
    }

    fn emergency_app() -> App<'static> {
        let mut app = App::new(&[]);
        app.screen = Screen::Emergency {
            message: "boot failed: test".to_string(),
            items: vec![
                EmergencyItem {
                    label: "Reboot",
                    choice: EmergencyChoice::Reboot,
                },
                EmergencyItem {
                    label: "Shell",
                    choice: EmergencyChoice::Shell,
                },
            ],
            selected: 0,
            chosen: None,
        };
        app
    }

    fn emergency_state(app: &App<'_>) -> (usize, Option<EmergencyChoice>) {
        match &app.screen {
            Screen::Emergency {
                selected, chosen, ..
            } => (*selected, *chosen),
            _ => panic!("expected Emergency screen"),
        }
    }

    #[test]
    fn emergency_arrow_keys_move_selection_within_bounds() {
        let mut app = emergency_app();
        assert_eq!(emergency_state(&app).0, 0);

        // Up at index 0 stays at 0.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(emergency_state(&app).0, 0);

        // Down advances.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Down at end stays at end.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Up walks back.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(emergency_state(&app).0, 0);

        // vi-keys also work.
        assert!(!app.on_key(press(KeyCode::Char('j'))));
        assert_eq!(emergency_state(&app).0, 1);
        assert!(!app.on_key(press(KeyCode::Char('k'))));
        assert_eq!(emergency_state(&app).0, 0);
    }

    #[test]
    fn emergency_enter_returns_selected_variant() {
        // selected=0 -> Reboot.
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Enter)));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

        // selected=1 -> Shell.
        let mut app = emergency_app();
        assert!(!app.on_key(press(KeyCode::Down)));
        assert!(app.on_key(press(KeyCode::Enter)));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Shell));
    }

    #[test]
    fn emergency_esc_preserves_selection_without_committing() {
        let mut app = emergency_app();
        // Move to Shell.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Esc must not commit and must not move.
        assert!(!app.on_key(press(KeyCode::Esc)));
        let (sel, chosen) = emergency_state(&app);
        assert_eq!(sel, 1, "selection must be preserved across Esc");
        assert!(chosen.is_none(), "Esc must not commit a choice");
    }

    #[test]
    fn emergency_hotkeys_r_and_s_commit_directly() {
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('r'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('s'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Shell));
    }

    #[test]
    fn boot_status_constructor_parks_app_on_boot_screen() {
        let app = App::boot_status("phase 0: kernel handoff");
        assert!(app.decision.is_none());
        match &app.screen {
            Screen::BootStatus(data) => {
                assert_eq!(&*data.phase, "phase 0: kernel handoff");
                assert!(data.log_lines.is_empty());
                assert_eq!(data.spinner_frame, 0);
            }
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_setters_mutate_in_place() {
        let mut app = App::boot_status("initial");
        app.set_boot_phase("phase 2");
        app.set_boot_log_lines(vec!["one".into(), "two".into()]);
        match &app.screen {
            Screen::BootStatus(data) => {
                assert_eq!(&*data.phase, "phase 2");
                assert_eq!(data.log_lines, vec!["one", "two"]);
            }
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_spinner_tick_wraps_modulo_frame_count() {
        let mut app = App::boot_status("waiting");
        for _ in 0..SPINNER_FRAMES {
            app.tick_boot_spinner();
        }
        // SPINNER_FRAMES ticks must wrap back to 0.
        match &app.screen {
            Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 0),
            _ => panic!("expected BootStatus screen"),
        }
        // One more tick lands on frame 1.
        app.tick_boot_spinner();
        match &app.screen {
            Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 1),
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_on_key_does_not_produce_decision() {
        let mut app = App::boot_status("phase X");
        // Any keypress is absorbed; no decision is emitted.
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('s'),
            KeyCode::Char('q'),
        ] {
            assert!(!app.on_key(press(code)), "{code:?} must not exit");
            assert!(app.decision.is_none(), "{code:?} must not set decision");
        }
    }

    #[test]
    fn boot_status_setters_are_noop_on_other_screens_in_release() {
        // In release builds the setters are no-ops on non-BootStatus
        // screens — debug_assert is stripped. We can't toggle the cfg
        // mid-test, but we can drive the same path via a small helper
        // that checks `let-else` branches don't panic when the
        // assertion is *expected* to fire only in debug builds. To
        // keep this test universally runnable, we run it only outside
        // debug_assertions.
        if cfg!(debug_assertions) {
            return;
        }
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens); // Screen::List
        app.set_boot_phase("ignored");
        app.set_boot_log_lines(vec!["ignored".into()]);
        app.tick_boot_spinner();
        // Screen must still be List, untouched.
        assert!(matches!(app.screen, Screen::List));
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
