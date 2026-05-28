//! Console picker dialog + shell-relay driver.
//!
//! When the operator selects `[Shell]` on the emergency screen, NMBL no
//! longer `execve(2)`s into the rescue shell as PID 1. Instead it:
//!
//! 1. Reads `/sys/class/tty/console/active` to determine the
//!    kernel-elected primary interactive console (the "active" tty).
//! 2. Renders a checkbox picker over the live [`Console`] listing
//!    `/dev/console`-resolved-target plus every `extra_consoles` entry
//!    from the runtime config. The active console is pre-checked; the
//!    operator may further narrow or expand the set.
//! 3. On `[Spawn]`, forks ONE busybox shell over a PTY pair via
//!    [`crate::sys::pty::spawn_shell`] and starts a multi-target
//!    multiplex relay loop. Bytes read from the PTY master fan-out to
//!    every selected target fd; bytes read from each target merge into
//!    the master's input.
//! 4. If the selected target set includes the device our [`Console`] is
//!    using for display, NMBL calls [`Console::suspend`] so the kernel
//!    (or kernel-VT-bound framebuffer) can paint the shell directly,
//!    then [`Console::resume`] when the shell exits.
//!    Otherwise the TUI shows a "Shell running on /dev/X" modal until
//!    bash exits.
//!
//! This module owns step 2 (the picker dialog state machine and renderer)
//! and step 4's selection logic; the actual relay-loop bytes pump lives
//! in [`crate::ui::console_relay`].
//!
//! ## Why no `Screen::ConsolePicker` variant?
//!
//! The picker is an entirely contained sub-flow: it never coexists with
//! the boot menu or the editing screen. Keeping its state local to this
//! module's driver loop (rather than in [`crate::ui::app::Screen`])
//! avoids growing the central state machine for a transient modal —
//! same pattern [`crate::ui::show_modal_error`] uses.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::sys::tty::read_active_console;
use crate::ui::POLL_SLICE;
use crate::ui::console::Console;

/// One row in the picker dialog. The label is the displayed
/// `/dev/<tty>` path (or the special form `"/dev/console (-> /dev/X)"`
/// for the active console alias); `target` is the concrete path NMBL
/// actually opens at relay time.
#[derive(Debug, Clone)]
pub struct PickerCandidate {
    /// What we show in the dialog (the operator-facing rendering).
    pub label: String,
    /// What we open for I/O when [`Spawn`] is committed. Always a
    /// `/dev/<tty>` path.
    ///
    /// [`Spawn`]: PickerOutcome::Spawn
    pub target: PathBuf,
    /// True when this candidate is the kernel-elected primary
    /// interactive console (resolved from
    /// `/sys/class/tty/console/active`). Pre-checked by default and
    /// can never be unchecked below zero — the dialog refuses to
    /// [`Spawn`] with an empty target set, so the active console is
    /// effectively mandatory if it's the only candidate.
    pub is_active: bool,
}

/// Mutable state behind the picker dialog. Owned by the driver loop;
/// callers don't see it because every entry point hides it behind a
/// composite outcome.
#[derive(Debug)]
pub struct PickerState {
    pub candidates: Vec<PickerCandidate>,
    /// Per-candidate checkbox state. Length matches `candidates`.
    pub selected: Vec<bool>,
    /// Highlighted row in the candidates list.
    pub cursor: usize,
    /// Which button at the bottom (Spawn / Cancel) is highlighted.
    /// Up/Down arrows wrap between the list and the buttons; Space
    /// toggles checkboxes only when cursor is on the list, and Enter
    /// fires on whichever button is highlighted.
    pub button_cursor: ButtonCursor,
    /// `None` until the operator commits — either [`Spawn`] with the
    /// chosen targets or [`Cancel`] to bail back to the emergency menu.
    pub outcome: Option<PickerOutcome>,
}

/// Which of the two bottom buttons is highlighted. The list rows
/// occupy "Focus::List(idx)" implicitly via `cursor`; this enum only
/// tracks which button has focus when `focus_zone == Buttons`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonCursor {
    Spawn,
    Cancel,
}

/// What the operator committed to. The driver loop returns this as
/// the public outcome of [`run_picker`].
#[derive(Debug)]
pub enum PickerOutcome {
    /// Spawn the shell with the (non-empty) set of selected `/dev/<tty>`
    /// targets. Always at least one entry.
    Spawn { targets: Vec<PathBuf> },
    /// Operator cancelled (Esc / [Cancel]). Caller re-shows the
    /// emergency menu.
    Cancel,
}

/// Where the keyboard cursor currently lives: in the candidate list or
/// on one of the two bottom buttons. Translates Up/Down navigation
/// rules without a separate state machine for each list cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusZone {
    List,
    Buttons,
}

impl PickerState {
    /// Construct picker state from a config + an injected active
    /// console resolver. The resolver pattern lets unit tests inject a
    /// fixture path without touching `/sys`.
    ///
    /// The returned state always contains at least one candidate when
    /// `active_console_resolver` succeeds: the active console itself.
    /// `extra_consoles` entries that DUPLICATE the active console's
    /// `/dev/<tty>` path are deduplicated — they would otherwise let
    /// the operator "double-multiplex" onto the same fd, which the
    /// relay loop tolerates but the operator does not benefit from.
    ///
    /// Returns `Err` when the active console cannot be determined AND
    /// no `extra_consoles` are configured: with no candidates the
    /// picker can't offer anything to spawn on.
    pub fn build<F>(config: &Config, active_console_resolver: F) -> Result<PickerState>
    where
        F: FnOnce() -> Result<PathBuf>,
    {
        let mut candidates: Vec<PickerCandidate> = Vec::new();
        let mut selected: Vec<bool> = Vec::new();

        // The active console resolver may fail (e.g. /sys missing in a
        // pathological initramfs). On failure we fall back to a fixed
        // `/dev/console` candidate so the operator at least gets the
        // historical behaviour rather than an empty picker.
        let active_path = match active_console_resolver() {
            Ok(p) => p,
            Err(e) => {
                nmbl_warn!(
                    "console picker: active-console resolver failed: {e}; \
                     falling back to /dev/console"
                );
                PathBuf::from("/dev/console")
            }
        };

        candidates.push(PickerCandidate {
            label: format!("/dev/console (-> {})", active_path.display()),
            target: active_path.clone(),
            is_active: true,
        });
        selected.push(true);

        // Extras: keep order, drop entries that resolve to the same
        // path as the active console. We do raw string compare on the
        // path; operators paste literal /dev/<tty> entries into Nix.
        for extra in &config.emergency_shell.extra_consoles {
            let extra_path = PathBuf::from(extra);
            if extra_path == active_path {
                continue;
            }
            candidates.push(PickerCandidate {
                label: extra.clone(),
                target: extra_path,
                is_active: false,
            });
            selected.push(false);
        }

        Ok(PickerState {
            candidates,
            selected,
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        })
    }

    /// Currently selected target set, in candidate order. Used by the
    /// renderer for the running-modal label and by the driver loop on
    /// [`Spawn`] commit.
    pub fn selected_targets(&self) -> Vec<PathBuf> {
        self.candidates
            .iter()
            .zip(self.selected.iter())
            .filter(|(_, on)| **on)
            .map(|(c, _)| c.target.clone())
            .collect()
    }

    /// True when no candidate is currently checked. Drives the
    /// renderer's button-greying and gates [`Spawn`] commit.
    pub fn nothing_selected(&self) -> bool {
        !self.selected.iter().any(|&on| on)
    }

    /// Where the navigation cursor lives logically: in the candidate
    /// list or on the buttons. This is derived from `cursor` — when
    /// `cursor == candidates.len()` the focus is on the buttons.
    fn focus(&self) -> FocusZone {
        if self.cursor >= self.candidates.len() {
            FocusZone::Buttons
        } else {
            FocusZone::List
        }
    }

    /// Reduce a [`KeyEvent`] into a state mutation. Returns `true` if
    /// the dialog wants to exit (i.e. `outcome` is now `Some`).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ignore release/repeat; on_key only acts on Press.
        if key.kind != KeyEventKind::Press {
            return self.outcome.is_some();
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor_up();
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor_down();
                false
            }
            // Left/Right toggles between the two buttons when focus is
            // on the button row.
            KeyCode::Left => {
                if self.focus() == FocusZone::Buttons {
                    self.button_cursor = ButtonCursor::Spawn;
                }
                false
            }
            KeyCode::Right => {
                if self.focus() == FocusZone::Buttons {
                    self.button_cursor = ButtonCursor::Cancel;
                }
                false
            }
            KeyCode::Char(' ') => {
                // Space toggles checkbox only when focus is on the list.
                if let FocusZone::List = self.focus()
                    && let Some(slot) = self.selected.get_mut(self.cursor)
                {
                    *slot = !*slot;
                }
                false
            }
            KeyCode::Enter => self.commit_focus(),
            KeyCode::Esc => {
                self.outcome = Some(PickerOutcome::Cancel);
                true
            }
            _ => false,
        }
    }

    /// Apply Enter on the currently-focused element. On the list, Enter
    /// toggles the checkbox (same as Space); on the buttons it commits
    /// the dialog.
    fn commit_focus(&mut self) -> bool {
        match self.focus() {
            FocusZone::List => {
                if let Some(slot) = self.selected.get_mut(self.cursor) {
                    *slot = !*slot;
                }
                false
            }
            FocusZone::Buttons => match self.button_cursor {
                ButtonCursor::Spawn => {
                    if self.nothing_selected() {
                        // Reject the commit: there is nothing to spawn
                        // on. Operator either ticks a box or cancels.
                        return false;
                    }
                    self.outcome = Some(PickerOutcome::Spawn {
                        targets: self.selected_targets(),
                    });
                    true
                }
                ButtonCursor::Cancel => {
                    self.outcome = Some(PickerOutcome::Cancel);
                    true
                }
            },
        }
    }

    /// Cursor goes up one row. Wraps from the buttons back to the last
    /// list row; saturates at row 0.
    fn move_cursor_up(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Cursor goes down. From the last list row it moves to the
    /// "buttons" pseudo-row; saturates there.
    fn move_cursor_down(&mut self) {
        // `cursor == candidates.len()` is the buttons row; that's the
        // last reachable cursor position.
        let last = self.candidates.len();
        if self.cursor < last {
            self.cursor = self.cursor.saturating_add(1);
        }
    }
}

/// Outcome of running the picker dialog. The relay-loop wrapper
/// surfaces this to the caller so the emergency-shell flow can react
/// (start the relay vs. drop back to the menu).
#[derive(Debug)]
pub enum PickerSessionOutcome {
    /// Operator chose targets and the relay loop ran to completion.
    /// The caller re-displays the emergency menu.
    ShellRan,
    /// Operator cancelled the dialog before spawning anything.
    Cancelled,
}

/// Run the picker dialog on `console` and, when committed, drive the
/// multi-target shell-relay loop. Returns to the caller after the
/// shell exits or the operator cancels.
///
/// The function NEVER produces a [`crate::terminal::TerminalAction`]:
/// NMBL stays at PID 1 throughout. This is the deliberate departure
/// from the legacy `EmergencyChoice::RawShell` -> execve path.
pub fn run_picker_session(
    console: &mut dyn Console,
    config: &Config,
) -> Result<PickerSessionOutcome> {
    let mut state = PickerState::build(config, read_active_console)?;
    if state.candidates.is_empty() {
        // Defence in depth: build() never returns an empty list today,
        // but a future refactor could. Skip the picker if there is
        // nothing to offer.
        return Ok(PickerSessionOutcome::Cancelled);
    }
    drive_picker_loop(&mut state, console)?;

    match state.outcome {
        Some(PickerOutcome::Spawn { targets }) => {
            crate::ui::console_relay::run_relay(console, config, &targets)?;
            Ok(PickerSessionOutcome::ShellRan)
        }
        Some(PickerOutcome::Cancel) | None => Ok(PickerSessionOutcome::Cancelled),
    }
}

/// Drive the render-poll-react loop until the picker commits an
/// outcome.
fn drive_picker_loop(state: &mut PickerState, console: &mut dyn Console) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render_picker(console, state)?;
            dirty = false;
        }
        if let Some(key) = console.poll_key(POLL_SLICE)? {
            let exited = state.on_key(key);
            dirty = true;
            if exited {
                return Ok(());
            }
        }
    }
}

/// Issue one frame paint via [`Console::draw_with`]. Keeping the
/// renderer behind a thin wrapper localises the borrow of `state` so
/// the closure stays `FnMut` and doesn't capture overlapping
/// references.
fn render_picker(console: &mut dyn Console, state: &PickerState) -> Result<()> {
    console.draw_with(&mut |frame| render_picker_frame(frame, state))
}

/// Pure render function — exported `pub(crate)` so the renderer can be
/// exercised by unit tests with a `TestBackend`.
pub(crate) fn render_picker_frame(frame: &mut Frame<'_>, state: &PickerState) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area);

    // Header: bold "Spawn shell on:" plus a hint about the active console.
    let header_para = Paragraph::new(Line::from(vec![Span::styled(
        "Spawn shell on:",
        Style::default().add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left);
    frame.render_widget(header_para, header);

    // Centred modal over the body so the dialog reads as a focus
    // shift rather than a full-screen replacement.
    let modal = centered_rect(body, 64, body.height.saturating_div(2).max(10));
    frame.render_widget(Clear, modal);

    // Layout inside the modal: candidate list + buttons row.
    let list_height = u16::try_from(state.candidates.len().saturating_add(2)).unwrap_or(u16::MAX);
    let [list_area, button_area] =
        Layout::vertical([Constraint::Length(list_height), Constraint::Length(3)]).areas::<2>(modal);

    let items: Vec<ListItem<'_>> = state
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let on = state.selected.get(i).copied().unwrap_or(false);
            let inner = if on { 'x' } else { ' ' };
            let suffix = if c.is_active { "  (active)" } else { "" };
            ListItem::new(Line::from(format!(
                "[{inner}]  {label}{suffix}",
                label = c.label,
            )))
        })
        .collect();

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("targets"))
        .highlight_style(highlight)
        .highlight_symbol("> ");

    let mut list_state = ListState::default();
    if !state.candidates.is_empty() && state.focus_for_render() == FocusZone::List {
        let last_idx = state.candidates.len().saturating_sub(1);
        list_state.select(Some(state.cursor.min(last_idx)));
    }
    frame.render_stateful_widget(list, list_area, &mut list_state);

    // Buttons row: [Spawn] [Cancel]. Each button is a Paragraph; the
    // focused one carries the highlight style.
    let [spawn_area, cancel_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas::<2>(button_area);

    let on_buttons = state.focus_for_render() == FocusZone::Buttons;
    let spawn_focused = on_buttons && state.button_cursor == ButtonCursor::Spawn;
    let cancel_focused = on_buttons && state.button_cursor == ButtonCursor::Cancel;

    let spawn_label = if state.nothing_selected() {
        "[Spawn (no target)]"
    } else {
        "[Spawn]"
    };
    let spawn_style = if spawn_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else if state.nothing_selected() {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    let spawn_para = Paragraph::new(Span::styled(spawn_label, spawn_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(spawn_para, spawn_area);

    let cancel_style = if cancel_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cancel_para = Paragraph::new(Span::styled("[Cancel]", cancel_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(cancel_para, cancel_area);

    let footer_text = "up/down move  Space toggle  Enter confirm  Esc cancel";
    frame.render_widget(
        Paragraph::new(footer_text).alignment(Alignment::Left),
        footer,
    );
}

impl PickerState {
    /// Surface a copy of the internal focus zone for the renderer
    /// (`FocusZone` is private to this module by design, but the
    /// renderer is in the same module so it can call this helper
    /// without leaking the enum publicly).
    fn focus_for_render(&self) -> FocusZone {
        self.focus()
    }
}

/// Centre a width×height rect inside `area`. Mirrors the same helper
/// in `view.rs`; duplicated here to keep this module self-contained
/// (the picker doesn't otherwise touch `view`).
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

/// True iff `display_target` is one of the operator's selected
/// targets. The relay path uses this to decide whether to suspend
/// the live [`Console`] (display overlap) or show the "shell running
/// elsewhere" modal (no overlap).
///
/// Both arguments are compared by their on-disk representation, so a
/// trailing slash or a symlink chase wouldn't match — operators paste
/// literal `/dev/<tty>` strings into Nix and the active-console
/// resolver also returns the literal form.
pub fn display_overlaps_targets(display_target: &Path, targets: &[PathBuf]) -> bool {
    targets.iter().any(|t| t == display_target)
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

    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::error::NmblError;
    use crate::ui::app::App;
    use crate::ui::console::{Console, ConsoleKind};

    /// Fake [`Console`] for driving the picker loop in tests. Returns
    /// a queued sequence of key events and remembers the last frame's
    /// candidate-count rendering for assertions.
    struct FakeConsole {
        events: std::collections::VecDeque<Option<KeyEvent>>,
        renders: u32,
    }

    impl FakeConsole {
        fn new(events: Vec<Option<KeyEvent>>) -> Self {
            Self {
                events: events.into(),
                renders: 0,
            }
        }
    }

    impl Console for FakeConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            Ok(self.events.pop_front().flatten())
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
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

    /// `build` with a fake resolver yielding `/dev/tty0` and a couple
    /// of extras must produce a candidate list with the active console
    /// pre-checked, extras unchecked, and no duplicates.
    #[test]
    fn build_active_console_is_pre_checked() {
        let mut cfg = Config::recovery_default();
        cfg.emergency_shell.extra_consoles =
            vec!["/dev/ttyS0".to_string(), "/dev/tty1".to_string()];
        let state =
            PickerState::build(&cfg, || Ok(PathBuf::from("/dev/tty0"))).expect("build");
        // Active console first, two extras next.
        assert_eq!(state.candidates.len(), 3);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/tty0"));
        assert!(state.candidates[0].is_active);
        assert!(state.selected[0], "active console must be pre-checked");

        assert_eq!(state.candidates[1].target, PathBuf::from("/dev/ttyS0"));
        assert!(!state.candidates[1].is_active);
        assert!(!state.selected[1], "extras must default to unchecked");

        assert_eq!(state.candidates[2].target, PathBuf::from("/dev/tty1"));
        assert!(!state.selected[2]);
    }

    #[test]
    fn build_deduplicates_extra_that_matches_active() {
        let mut cfg = Config::recovery_default();
        cfg.emergency_shell.extra_consoles = vec!["/dev/ttyS0".to_string()];
        let state = PickerState::build(&cfg, || Ok(PathBuf::from("/dev/ttyS0"))).expect("build");
        // The duplicate must collapse to a single candidate — the
        // active console.
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/ttyS0"));
        assert!(state.candidates[0].is_active);
    }

    #[test]
    fn build_falls_back_to_dev_console_when_resolver_errors() {
        // A pathological initramfs without /sys still needs to offer
        // *some* candidate so the operator can launch a shell.
        let cfg = Config::recovery_default();
        let state = PickerState::build(&cfg, || {
            Err(NmblError::Tui {
                source: std::io::Error::other("no /sys"),
            })
        })
        .expect("fallback build must still succeed");
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].target, PathBuf::from("/dev/console"));
        assert!(state.candidates[0].is_active);
    }

    #[test]
    fn space_toggles_checkbox_on_focused_row() {
        let mut state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "a".into(),
                    target: PathBuf::from("/dev/a"),
                    is_active: true,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    is_active: false,
                },
            ],
            selected: vec![true, false],
            cursor: 1,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };
        // Space on row 1 (b) toggles it on.
        assert!(!state.on_key(press(KeyCode::Char(' '))));
        assert!(state.selected[1]);
        // Toggling again turns it off.
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
                    is_active: true,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    is_active: false,
                },
            ],
            selected: vec![true, true],
            cursor: 2, // = candidates.len() → buttons row
            button_cursor: ButtonCursor::Spawn,
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
        // Operator unticked the only candidate, then pressed Enter on
        // [Spawn]. The dialog must NOT commit; outcome stays None so
        // the loop renders one more frame and waits.
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                is_active: true,
            }],
            selected: vec![false],
            cursor: 1,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };
        assert!(!state.on_key(press(KeyCode::Enter)));
        assert!(state.outcome.is_none(), "empty-target Spawn must not commit");
    }

    #[test]
    fn esc_commits_cancel() {
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "a".into(),
                target: PathBuf::from("/dev/a"),
                is_active: true,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };
        assert!(state.on_key(press(KeyCode::Esc)));
        assert!(matches!(state.outcome, Some(PickerOutcome::Cancel)));
    }

    #[test]
    fn cursor_navigation_walks_through_list_and_buttons() {
        let mut state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "a".into(),
                    target: PathBuf::from("/dev/a"),
                    is_active: true,
                },
                PickerCandidate {
                    label: "b".into(),
                    target: PathBuf::from("/dev/b"),
                    is_active: false,
                },
            ],
            selected: vec![true, false],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };

        // Start at 0, Down moves to 1.
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.cursor, 1);
        // Down again moves to buttons (cursor == len).
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.cursor, 2);
        assert_eq!(state.focus_for_render(), FocusZone::Buttons);
        // Down at buttons saturates.
        state.on_key(press(KeyCode::Down));
        assert_eq!(state.cursor, 2);
        // Right switches button focus.
        state.on_key(press(KeyCode::Right));
        assert_eq!(state.button_cursor, ButtonCursor::Cancel);
        // Up walks back into the list.
        state.on_key(press(KeyCode::Up));
        assert_eq!(state.cursor, 1);
        assert_eq!(state.focus_for_render(), FocusZone::List);
    }

    #[test]
    fn driver_loop_runs_picker_to_spawn_outcome_via_fake_console() {
        // End-to-end: build a state, drive it with a scripted FakeConsole,
        // and assert the outcome the driver returns.
        let mut state = PickerState {
            candidates: vec![PickerCandidate {
                label: "/dev/tty0".into(),
                target: PathBuf::from("/dev/tty0"),
                is_active: true,
            }],
            selected: vec![true],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };
        let mut console = FakeConsole::new(vec![
            // Move cursor down to the buttons row.
            Some(press(KeyCode::Down)),
            // Enter on [Spawn].
            Some(press(KeyCode::Enter)),
        ]);
        drive_picker_loop(&mut state, &mut console).expect("loop must not error");
        match state.outcome {
            Some(PickerOutcome::Spawn { targets }) => {
                assert_eq!(targets, vec![PathBuf::from("/dev/tty0")]);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert!(console.renders >= 1);
    }

    #[test]
    fn renderer_paints_candidate_labels_and_buttons() {
        let state = PickerState {
            candidates: vec![
                PickerCandidate {
                    label: "/dev/console (-> /dev/tty0)".into(),
                    target: PathBuf::from("/dev/tty0"),
                    is_active: true,
                },
                PickerCandidate {
                    label: "/dev/ttyS0".into(),
                    target: PathBuf::from("/dev/ttyS0"),
                    is_active: false,
                },
            ],
            selected: vec![true, false],
            cursor: 0,
            button_cursor: ButtonCursor::Spawn,
            outcome: None,
        };
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        term.draw(|f| render_picker_frame(f, &state)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            dump.contains("Spawn shell on:"),
            "header must be visible: \n{dump}"
        );
        assert!(
            dump.contains("/dev/console"),
            "active console label must be visible: \n{dump}"
        );
        assert!(
            dump.contains("/dev/ttyS0"),
            "extra-console label must be visible: \n{dump}"
        );
        assert!(
            dump.contains("[Spawn]"),
            "Spawn button must be visible: \n{dump}"
        );
        assert!(
            dump.contains("[Cancel]"),
            "Cancel button must be visible: \n{dump}"
        );
    }

    #[test]
    fn display_overlaps_targets_matches_path() {
        let targets = vec![PathBuf::from("/dev/tty0"), PathBuf::from("/dev/ttyS0")];
        assert!(display_overlaps_targets(Path::new("/dev/tty0"), &targets));
        assert!(!display_overlaps_targets(Path::new("/dev/tty1"), &targets));
        assert!(!display_overlaps_targets(Path::new("/dev/tty0"), &[]));
    }
}
